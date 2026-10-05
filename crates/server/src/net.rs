//! The server's network side: a tokio thread that owns the QUIC endpoint
//! and talks to the host's event loop through two channels.
//!
//! One client is served at a time. A new connection takes over from the
//! current one, which is closed; the session itself carries on (§6).
//! Messages are tagged with the connection they belong to, so nothing meant
//! for an old connection reaches a new one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::{EncodedFrame, Packetizer};
use farsight_net::quinn::{self, Connection};
use farsight_net::sched::{Priority, Scheduler};
use farsight_net::{auth, endpoint, stream};
use farsight_proto::audio::AudioConfig;
use farsight_proto::control::{ClientMessage, Hello, ServerMessage};
use farsight_proto::datagram::{self, Datagram, Pong};
use farsight_proto::tiles::TilesHeader;
use farsight_proto::input::InputPacket;
use smithay::reexports::calloop::channel::Sender as HostSender;
use tokio::sync::mpsc;

/// Identifies one client connection.
pub type ConnId = u64;

/// From the network to the host.
#[derive(Debug)]
pub enum ToHost {
    /// A client said hello. The host answers with a `Welcome` and a
    /// keyframe.
    Connected(ConnId, Hello),
    Message(ConnId, ClientMessage),
    Input(ConnId, InputPacket),
    Disconnected(ConnId),
}

/// From the host (or the encode thread) to the network.
#[derive(Debug)]
pub enum ToNet {
    Frame(ConnId, Frame),
    Tiles(ConnId, Tiles),
    Message(ConnId, ServerMessage),
    /// An audio datagram, for whichever client listens.
    Audio(Bytes),
    /// The session is ending: close every connection with this reason.
    Shutdown(String),
}

#[derive(Debug)]
pub struct Frame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub epoch: u16,
    pub capture_us: u64,
    pub encode_us: u32,
}

/// One tiles update, ready to go.
#[derive(Debug)]
pub struct Tiles {
    pub update: crate::encode::tiles::Update,
    pub epoch: u16,
    pub capture_us: u64,
    pub encode_us: u32,
}

/// The session's audio, shared with the speaker's threads.
#[derive(Default)]
pub struct Audio {
    /// Set once the speaker is up.
    pub config: OnceLock<AudioConfig>,
    /// A client is listening: the speaker encodes and sends.
    pub listening: AtomicBool,
}

/// The application close code for a client whose key isn't authorized.
pub const CLOSE_UNAUTHORIZED: u32 = 3;

pub struct Options {
    pub port: u16,
    /// Client keys allowed in (§6), read on every connection.
    pub authorized_keys: std::path::PathBuf,
    pub identity: endpoint::Identity,
    /// Video is paced at this rate until there is a congestion controller
    /// (M4).
    pub rate_bps: u64,
    /// The host's clock, which pongs and frame timestamps are in.
    pub start: Instant,
    pub audio: Arc<Audio>,
}

/// The current connection, if any.
struct Current {
    id: ConnId,
    conn: Connection,
    sched: Scheduler,
    control: mpsc::UnboundedSender<ServerMessage>,
    /// The client plays audio and hasn't muted it.
    audio: bool,
}

/// Starts the network thread and returns where to send it frames and
/// messages.
pub fn spawn(opts: Options, host: HostSender<ToHost>) -> anyhow::Result<mpsc::UnboundedSender<ToNet>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("farsight-net")
        .enable_all()
        .build()?;
    let endpoint = {
        let _guard = rt.enter();
        endpoint::server(opts.port, &opts.identity)?
    };
    tracing::info!(
        addr = %endpoint.local_addr()?, fingerprint = opts.identity.fingerprint(),
        rate_mbps = opts.rate_bps / 1_000_000, "listening"
    );
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new().name("farsight-net".into()).spawn(move || {
        rt.block_on(run(endpoint, opts, host, rx));
    })?;
    Ok(tx)
}

/// What every connection's task shares.
struct Shared {
    opts: Options,
    host: HostSender<ToHost>,
    current: Mutex<Option<Current>>,
}

async fn run(endpoint: quinn::Endpoint, opts: Options, host: HostSender<ToHost>, rx: mpsc::UnboundedReceiver<ToNet>) {
    let shared = Arc::new(Shared { opts, host, current: Mutex::default() });
    tokio::spawn(dispatch(rx, shared.clone()));
    let mut next_id: ConnId = 0;
    while let Some(incoming) = endpoint.accept().await {
        next_id += 1;
        let (id, shared) = (next_id, shared.clone());
        tokio::spawn(async move {
            let addr = incoming.remote_address();
            match serve(id, incoming, &shared).await {
                Ok(()) => tracing::info!(id, %addr, "client gone"),
                Err(err) => tracing::info!(id, %addr, "client gone: {err:#}"),
            }
            let mut cur = shared.current.lock().unwrap();
            if cur.as_ref().is_some_and(|c| c.id == id) {
                *cur = None;
                shared.opts.audio.listening.store(false, Ordering::Relaxed);
            }
            let _ = shared.host.send(ToHost::Disconnected(id));
        });
    }
}

/// Routes the host's output to the current connection.
async fn dispatch(mut rx: mpsc::UnboundedReceiver<ToNet>, shared: Arc<Shared>) {
    let mut packetizer = Packetizer::new();
    let mut next_update: u32 = 0;
    while let Some(msg) = rx.recv().await {
        let cur = shared.current.lock().unwrap();
        let Some(c) = cur.as_ref() else { continue };
        match msg {
            ToNet::Shutdown(reason) => {
                c.conn.close(2u32.into(), reason.as_bytes());
            }
            ToNet::Frame(id, f) if id == c.id => {
                let Some(max) = c.conn.max_datagram_size() else { continue };
                let frame = EncodedFrame {
                    data: &f.data,
                    keyframe: f.keyframe,
                    epoch: f.epoch,
                    capture_us: f.capture_us,
                    encode_us: f.encode_us,
                };
                c.sched.send_frame(packetizer.packetize(&frame, max), f.keyframe);
            }
            ToNet::Tiles(id, t) if id == c.id => {
                let count = t.update.bodies.len();
                if count == 0 {
                    continue;
                }
                let mut header = TilesHeader {
                    epoch: t.epoch,
                    update: next_update,
                    index: 0,
                    count: count.min(u16::MAX as usize) as u16,
                    capture_us: t.capture_us,
                    encode_us: t.encode_us,
                    bounds: t.update.bounds,
                };
                next_update = next_update.wrapping_add(1);
                let datagrams = t
                    .update
                    .bodies
                    .iter()
                    .take(u16::MAX as usize)
                    .enumerate()
                    .map(|(i, body)| {
                        header.index = i as u16;
                        let mut out = Vec::new();
                        datagram::encode_tiles(&header, body, &mut out);
                        Bytes::from(out)
                    })
                    .collect();
                // Never superseded: each update carries only what changed.
                c.sched.send_frame(datagrams, false);
            }
            ToNet::Message(id, m) if id == c.id => {
                let _ = c.control.send(m);
            }
            ToNet::Audio(d) if c.audio => c.sched.send(Priority::Audio, d),
            _ => {} // for a connection that has gone
        }
    }
}

async fn serve(id: ConnId, incoming: quinn::Incoming, shared: &Shared) -> anyhow::Result<()> {
    let (host, current, audio) = (&shared.host, &shared.current, &shared.opts.audio);
    let (authorized_keys, rate_bps, start) = (&shared.opts.authorized_keys, shared.opts.rate_bps, shared.opts.start);
    let conn = incoming.await.context("handshake")?;
    tracing::info!(id, addr = %conn.remote_address(), "client connected");
    let (mut send, mut recv) = conn.accept_bi().await.context("waiting for the control stream")?;
    let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await? else {
        anyhow::bail!("the client did not start with Hello");
    };
    let key = hello.auth.key;
    let refuse = |why: &str| {
        conn.close(CLOSE_UNAUTHORIZED.into(), why.as_bytes());
        anyhow::anyhow!("refused: {why}")
    };
    if !auth::verify(&conn, &key, &hello.auth.signature) {
        return Err(refuse("the client's signature doesn't match its key"));
    }
    match auth::is_authorized(authorized_keys, &key) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                id, file = %authorized_keys.display(), line = auth::format_line(&key, ""),
                "a client key that isn't authorized; add its line to the file to let it in"
            );
            return Err(refuse(&format!("this client's key is not in {} on the server", authorized_keys.display())));
        }
        Err(err) => {
            tracing::error!("{err:#}");
            return Err(refuse("the server can't read its authorized keys"));
        }
    }
    tracing::info!(id, layout = ?hello.layout, decoders = hello.decoders.len(), "hello");

    let sched = Scheduler::spawn(conn.clone(), rate_bps, endpoint::DATAGRAM_BUFFER);
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    // Audio goes to a client that plays it, from a session that has it.
    let audio_config = hello.audio.and(audio.config.get().copied());
    let next = Current { id, conn: conn.clone(), sched: sched.clone(), control: control_tx, audio: audio_config.is_some() };
    let previous = current.lock().unwrap().replace(next);
    audio.listening.store(audio_config.is_some(), Ordering::Relaxed);
    if let Some(prev) = previous {
        tracing::info!(id, previous = prev.id, "taking over the session");
        prev.conn.close(1u32.into(), b"another client took over");
    }
    let _ = host.send(ToHost::Connected(id, hello));

    let writer = async {
        while let Some(msg) = control_rx.recv().await {
            stream::send(&mut send, &msg).await?;
            // The audio format follows the welcome, which comes first.
            if let (ServerMessage::Welcome(_), Some(config)) = (&msg, audio_config) {
                stream::send(&mut send, &ServerMessage::AudioConfig(config)).await?;
            }
        }
        anyhow::Ok(())
    };
    let reader = async {
        while let Some(msg) = stream::recv::<ClientMessage>(&mut recv).await? {
            if let ClientMessage::SetAudio { play } = msg {
                let mut cur = current.lock().unwrap();
                if let Some(c) = cur.as_mut().filter(|c| c.id == id) {
                    c.audio = play && audio_config.is_some();
                    audio.listening.store(c.audio, Ordering::Relaxed);
                    tracing::info!(id, play, "audio");
                }
                continue;
            }
            let _ = host.send(ToHost::Message(id, msg));
        }
        anyhow::Ok(())
    };
    let datagrams = async {
        let err = loop {
            let d = match conn.read_datagram().await {
                Ok(d) => d,
                Err(err) => break err,
            };
            match Datagram::decode(&d) {
                Some(Datagram::Input(p)) => {
                    let _ = host.send(ToHost::Input(id, p));
                }
                Some(Datagram::Ping(p)) => {
                    let pong = Pong { client_us: p.client_us, server_us: start.elapsed().as_micros() as u64 };
                    sched.send(Priority::Input, Bytes::from(Datagram::Pong(pong).to_vec()));
                }
                _ => {}
            }
        };
        Err::<(), _>(anyhow::Error::from(err))
    };
    tokio::select! {
        r = writer => r,
        r = reader => r,
        r = datagrams => r,
    }
}
