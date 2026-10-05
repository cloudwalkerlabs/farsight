//! Plaintext mode (`docs/design.md` §1): QUIC with a null crypto layer, for
//! networks that already encrypt and authenticate every packet (Tailscale,
//! WireGuard). Streams, datagrams, retransmission, migration and congestion
//! control are all still QUIC's; only the keys do nothing.
//!
//! The null handshake carries the transport parameters, a random value from
//! each side and the identity `farsight-plain/N` (N follows ALPN):
//!
//! ```text
//! client → server  Initial    HELLO     identity, client random, parameters
//! server → client  Initial    ACCEPT    server random
//!                  Handshake  PARAMS    parameters
//! client → server  Handshake  FINISHED
//! ```
//!
//! The two randoms make each connection's exported keying material unique,
//! so a client key's signature (`crate::auth`) still proves the key on this
//! connection, against a fresh server nonce. Nothing protects the
//! connection after that: the network must.
//!
//! Plaintext runs on a QUIC version of its own, so a TLS endpoint and a
//! plaintext one fail at once with version negotiation instead of
//! misreading each other's packets.

use std::any::Any;
use std::sync::Arc;

use bytes::BytesMut;
use quinn_proto::crypto::{
    self, CryptoError, ExportKeyingMaterialError, HeaderKey, KeyPair, Keys, PacketKey, UnsupportedVersion,
};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectError, ConnectionId, Side, TransportError, TransportErrorCode};
use ring::rand::{SecureRandom, SystemRandom};

/// The QUIC version plaintext endpoints speak: "FSP" and 0.
pub const VERSION: u32 = 0x4653_5000;

/// Carried in HELLO, as ALPN is in TLS; its number follows
/// [`farsight_proto::ALPN`]'s.
pub const IDENTITY: &[u8] = b"farsight-plain/4";

const HELLO: u8 = 1;
const ACCEPT: u8 = 2;
const PARAMS: u8 = 3;
const FINISHED: u8 = 4;

/// The largest handshake message accepted.
const MAX_MESSAGE: usize = 4096;

struct NullHeader;

impl HeaderKey for NullHeader {
    fn decrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}
    fn encrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}
    fn sample_size(&self) -> usize {
        0
    }
}

struct NullPacket;

impl PacketKey for NullPacket {
    fn encrypt(&self, _packet: u64, _buf: &mut [u8], _header_len: usize) {}
    fn decrypt(&self, _packet: u64, _header: &[u8], _payload: &mut BytesMut) -> Result<(), CryptoError> {
        Ok(())
    }
    fn tag_len(&self) -> usize {
        0
    }
    fn confidentiality_limit(&self) -> u64 {
        u64::MAX
    }
    fn integrity_limit(&self) -> u64 {
        u64::MAX
    }
}

fn null_keys() -> Keys {
    Keys {
        header: KeyPair { local: Box::new(NullHeader), remote: Box::new(NullHeader) },
        packet: KeyPair { local: Box::new(NullPacket), remote: Box::new(NullPacket) },
    }
}

fn violation(reason: &str) -> TransportError {
    TransportError {
        code: TransportErrorCode::crypto(0x78), // no_application_protocol
        frame: None,
        reason: reason.into(),
    }
}

/// Where the handshake is, on either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Client: HELLO to write. Server: waiting for HELLO.
    Start,
    /// Client: HELLO sent, waiting for ACCEPT. Server: ACCEPT to write.
    Hello,
    /// Client: ACCEPT read; handshake keys to hand over. Server: PARAMS
    /// to write, with 1-RTT keys.
    Accept,
    /// Client: waiting for PARAMS. Server: waiting for FINISHED.
    Params,
    /// Client: FINISHED to write, with 1-RTT keys.
    Finish,
    Done,
}

struct Session {
    side: Side,
    step: Step,
    local_params: TransportParameters,
    peer_params: Option<TransportParameters>,
    client_random: [u8; 32],
    server_random: [u8; 32],
    /// Handshake bytes received and not yet parsed.
    inbox: Vec<u8>,
    data_ready: bool,
}

impl Session {
    fn new(side: Side, params: &TransportParameters) -> Self {
        let mut random = [0; 32];
        SystemRandom::new().fill(&mut random).expect("the system's random source");
        let (client_random, server_random) = match side {
            Side::Client => (random, [0; 32]),
            Side::Server => ([0; 32], random),
        };
        Self {
            side,
            step: Step::Start,
            local_params: *params,
            peer_params: None,
            client_random,
            server_random,
            inbox: Vec::new(),
            data_ready: false,
        }
    }

    fn write_message(buf: &mut Vec<u8>, kind: u8, body: &[u8]) {
        buf.push(kind);
        buf.extend_from_slice(&(body.len() as u16).to_be_bytes());
        buf.extend_from_slice(body);
    }

    fn params_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.local_params.write(&mut out);
        out
    }

    fn read_params(&mut self, body: &[u8]) -> Result<(), TransportError> {
        let params = TransportParameters::read(self.side, &mut std::io::Cursor::new(body))
            .map_err(|_| violation("malformed transport parameters"))?;
        self.peer_params = Some(params);
        Ok(())
    }

    fn handle(&mut self, kind: u8, body: &[u8]) -> Result<(), TransportError> {
        match (self.side, self.step, kind) {
            (Side::Server, Step::Start, HELLO) => {
                let id_len = *body.first().ok_or_else(|| violation("short HELLO"))? as usize;
                let id = body.get(1..1 + id_len).ok_or_else(|| violation("short HELLO"))?;
                if id != IDENTITY {
                    return Err(violation("not a farsight plaintext client"));
                }
                let rest = &body[1 + id_len..];
                if rest.len() < 32 {
                    return Err(violation("short HELLO"));
                }
                self.client_random.copy_from_slice(&rest[..32]);
                self.read_params(&rest[32..])?;
                self.step = Step::Hello;
                self.data_ready = true;
            }
            (Side::Client, Step::Hello, ACCEPT) => {
                if body.len() != 32 {
                    return Err(violation("bad ACCEPT"));
                }
                self.server_random.copy_from_slice(body);
                self.step = Step::Accept;
            }
            (Side::Client, Step::Params, PARAMS) => {
                self.read_params(body)?;
                self.step = Step::Finish;
                self.data_ready = true;
            }
            (Side::Server, Step::Params, FINISHED) => self.step = Step::Done,
            _ => return Err(violation("unexpected plaintext handshake message")),
        }
        Ok(())
    }
}

impl crypto::Session for Session {
    fn initial_keys(&self, _dst_cid: &ConnectionId, _side: Side) -> Keys {
        null_keys()
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        self.data_ready.then(|| Box::new(IDENTITY) as Box<dyn Any>)
    }

    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        None
    }

    fn early_crypto(&self) -> Option<(Box<dyn HeaderKey>, Box<dyn PacketKey>)> {
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        None
    }

    fn is_handshaking(&self) -> bool {
        self.step != Step::Done
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        let was_ready = self.data_ready;
        self.inbox.extend_from_slice(buf);
        loop {
            if self.inbox.len() < 3 {
                break;
            }
            let len = u16::from_be_bytes([self.inbox[1], self.inbox[2]]) as usize;
            if len > MAX_MESSAGE {
                return Err(violation("oversized plaintext handshake message"));
            }
            if self.inbox.len() < 3 + len {
                break;
            }
            let message: Vec<u8> = self.inbox.drain(..3 + len).collect();
            self.handle(message[0], &message[3..])?;
        }
        Ok(self.data_ready && !was_ready)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        Ok(self.peer_params)
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        match (self.side, self.step) {
            (Side::Client, Step::Start) => {
                let mut body = vec![IDENTITY.len() as u8];
                body.extend_from_slice(IDENTITY);
                body.extend_from_slice(&self.client_random);
                body.extend_from_slice(&self.params_bytes());
                Self::write_message(buf, HELLO, &body);
                self.step = Step::Hello;
                None
            }
            // ACCEPT in Initial, then the handshake keys.
            (Side::Server, Step::Hello) => {
                Self::write_message(buf, ACCEPT, &self.server_random);
                self.step = Step::Accept;
                Some(null_keys())
            }
            // PARAMS in Handshake, then the 1-RTT keys.
            (Side::Server, Step::Accept) => {
                Self::write_message(buf, PARAMS, &self.params_bytes());
                self.step = Step::Params;
                Some(null_keys())
            }
            (Side::Client, Step::Accept) => {
                self.step = Step::Params;
                Some(null_keys())
            }
            (Side::Client, Step::Finish) => {
                Self::write_message(buf, FINISHED, &[]);
                self.step = Step::Done;
                Some(null_keys())
            }
            _ => None,
        }
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Box<dyn PacketKey>>> {
        Some(KeyPair { local: Box::new(NullPacket), remote: Box::new(NullPacket) })
    }

    fn is_valid_retry(&self, _orig_dst_cid: &ConnectionId, _header: &[u8], _payload: &[u8]) -> bool {
        true
    }

    /// HKDF-SHA256 over both randoms, as TLS exporters derive from the
    /// handshake.
    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        if self.client_random == [0; 32] || self.server_random == [0; 32] {
            return Err(ExportKeyingMaterialError);
        }
        let ikm = [self.client_random, self.server_random].concat();
        let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, IDENTITY).extract(&ikm);
        struct Len(usize);
        impl ring::hkdf::KeyType for Len {
            fn len(&self) -> usize {
                self.0
            }
        }
        let info = [label, context];
        prk.expand(&info, Len(output.len()))
            .and_then(|okm| okm.fill(output))
            .map_err(|_| ExportKeyingMaterialError)
    }
}

/// The client side of plaintext mode.
pub struct ClientConfig;

impl crypto::ClientConfig for ClientConfig {
    fn start_session(
        self: Arc<Self>,
        version: u32,
        _server_name: &str,
        params: &TransportParameters,
    ) -> Result<Box<dyn crypto::Session>, ConnectError> {
        if version != VERSION {
            return Err(ConnectError::UnsupportedVersion);
        }
        Ok(Box::new(Session::new(Side::Client, params)))
    }
}

/// The server side of plaintext mode.
pub struct ServerConfig;

impl crypto::ServerConfig for ServerConfig {
    fn initial_keys(&self, version: u32, _dst_cid: &ConnectionId) -> Result<Keys, UnsupportedVersion> {
        if version != VERSION {
            return Err(UnsupportedVersion);
        }
        Ok(null_keys())
    }

    fn retry_tag(&self, _version: u32, _orig_dst_cid: &ConnectionId, _packet: &[u8]) -> [u8; 16] {
        [0; 16]
    }

    fn start_session(self: Arc<Self>, _version: u32, params: &TransportParameters) -> Box<dyn crypto::Session> {
        Box::new(Session::new(Side::Server, params))
    }
}
