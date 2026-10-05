//! QUIC endpoints (`docs/design.md` §1). TLS 1.3 with a self-signed server
//! certificate. The client accepts any certificate in the handshake and
//! then checks its fingerprint against the one pinned on first use
//! ([`crate::auth::KnownHosts`]), before it sends anything.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use farsight_proto::ALPN;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, EndpointConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// quinn's datagram buffers. Outgoing video waits in the scheduler
/// ([`crate::sched`]), not here; this is room for a burst of datagrams that
/// arrive faster than the application reads them.
pub const DATAGRAM_BUFFER: usize = 2 << 20;

/// The longest a peer may hold back an ack.
pub const MAX_ACK_DELAY: Duration = Duration::from_millis(2);

/// The server's certificate and key.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn generate() -> anyhow::Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["farsight".into()])?;
        Ok(Self { cert: cert.cert.der().clone(), key: PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()) })
    }

    /// Loads `cert.der` and `key.der` from `dir`, creating them first if
    /// they don't exist, so the fingerprint stays the same across runs.
    pub fn load_or_generate(dir: &Path) -> anyhow::Result<Self> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Self { cert: CertificateDer::from(cert), key: PrivatePkcs8KeyDer::from(key) });
        }
        let id = Self::generate()?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&cert_path, &id.cert)?;
        write_private(&key_path, id.key.secret_pkcs8_der())?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

#[cfg(unix)]
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?.write_all(data)
}

#[cfg(not(unix))]
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

/// SHA-256 of a DER certificate, as colon-separated hex.
pub fn fingerprint(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    digest.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Tuned for interactive media rather than bulk transfer.
///
/// - **The MTU stays at QUIC's minimum of 1200**, as media stacks do: path
///   MTU discovery's black-hole detection takes random loss for a black
///   hole, and when the MTU shrinks quinn drops every queued datagram that
///   no longer fits, which is a whole frame.
/// - **Congestion control is farsight's own** ([`crate::cc`]), and the peer
///   is asked to ack within [`MAX_ACK_DELAY`], so round trips measure the
///   path rather than the peer's ack timer.
/// - `FARSIGHT_NO_GSO` turns UDP segmentation offload off. Only tests want
///   it: `tc netem` drops an offloaded batch of packets whole, where a real
///   link loses them one by one (tools/m4/netem.sh).
pub fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_secs(1)))
        .datagram_receive_buffer_size(Some(DATAGRAM_BUFFER))
        .datagram_send_buffer_size(DATAGRAM_BUFFER)
        .mtu_discovery_config(None)
        .congestion_controller_factory(Arc::new(crate::cc::Factory));
    let mut acks = quinn::AckFrequencyConfig::default();
    acks.max_ack_delay(Some(MAX_ACK_DELAY));
    t.ack_frequency_config(Some(acks));
    if std::env::var_os("FARSIGHT_NO_GSO").is_some() {
        t.enable_segmentation_offload(false);
    }
    t
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Where a server listens: every address (IPv4 too) unless given one.
fn listen_addr(listen: Option<IpAddr>, port: u16) -> SocketAddr {
    SocketAddr::from((listen.unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED)), port))
}

/// A server endpoint on `port`.
pub fn server(listen: Option<IpAddr>, port: u16, id: &Identity) -> anyhow::Result<Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], PrivateKeyDer::Pkcs8(id.key.clone_key()))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport_config()));
    let addr = listen_addr(listen, port);
    Endpoint::server(config, addr).with_context(|| format!("binding UDP {addr}"))
}

/// A plaintext server endpoint (§1) on `listen`, which must be given.
pub fn server_plain(listen: IpAddr, port: u16) -> anyhow::Result<Endpoint> {
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crate::plain::ServerConfig));
    config.transport_config(Arc::new(transport_config()));
    let mut endpoint_config = EndpointConfig::default();
    endpoint_config.supported_versions(vec![crate::plain::VERSION]);
    let addr = listen_addr(Some(listen), port);
    let socket = std::net::UdpSocket::bind(addr).with_context(|| format!("binding UDP {addr}"))?;
    Ok(Endpoint::new(endpoint_config, Some(config), socket, Arc::new(quinn::TokioRuntime))?)
}

/// A client endpoint on an ephemeral port, in TLS or plaintext mode.
pub fn client(plain: bool) -> anyhow::Result<Endpoint> {
    let mut config = if plain {
        let mut c = quinn::ClientConfig::new(Arc::new(crate::plain::ClientConfig));
        c.version(crate::plain::VERSION);
        c
    } else {
        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(
                rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?))
    };
    config.transport_config(Arc::new(transport_config()));
    let mut endpoint_config = EndpointConfig::default();
    if plain {
        endpoint_config.supported_versions(vec![crate::plain::VERSION]);
    }
    let socket = std::net::UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))?;
    let mut endpoint = Endpoint::new(endpoint_config, None, socket, Arc::new(quinn::TokioRuntime))?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// Connects and returns the connection with the server's certificate
/// fingerprint, or `None` in plaintext mode.
pub async fn connect(endpoint: &Endpoint, addr: SocketAddr) -> anyhow::Result<(Connection, Option<String>)> {
    let conn = endpoint.connect(addr, "farsight")?.await.with_context(|| format!("connecting to {addr}"))?;
    let fp = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|certs| certs.first().map(|c| fingerprint(c)));
    Ok((conn, fp))
}

/// Accepts any certificate, but still checks that the server holds its key.
#[derive(Debug)]
struct AnyCert(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}
