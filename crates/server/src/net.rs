//! The server's network side: a tokio thread that owns the QUIC endpoint
//! and talks to the host's event loop through two channels.
//!
//! One client controls the session at a time: a new controlling client
//! takes over, and the previous one is closed. Any number of others may
//! watch, view-only (§6). Video, tiles, cursor and audio go to every
//! connection alike, encoded once; replies go to the connection they answer.
//! The session itself outlives them all.

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
use farsight_proto::control::{ClientMessage, ClipboardRequest, Hello, MAX_CLIPBOARD, ServerMessage, close};
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
    /// The client wants the session's clipboard; the reply is `None` if
    /// it changed.
    ClipboardRead(ConnId, ClipboardRequest, tokio::sync::oneshot::Sender<Option<Vec<u8>>>),
    Disconnected(ConnId),
}

/// From the host (or the encode thread) to the network.
#[derive(Debug)]
pub enum ToNet {
    /// Video and tiles, for every connection.
    Frame(Frame),
    Tiles(Tiles),
    /// For one connection.
    Message(ConnId, ServerMessage),
    /// For every connection.
    Broadcast(ServerMessage),
    /// An audio datagram, for whichever client listens.
    Audio(Bytes),
    /// An app in the session pastes: fetch the client's clipboard into the
    /// pipe.
    FetchClipboard(ConnId, crate::clipboard::Send),
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

pub struct Options {
    pub listen: Option<std::net::IpAddr>,
    /// Plaintext mode (§1); needs `listen`.
    pub plain: bool,
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

/// A client that said hello.
struct Conn {
    id: ConnId,
    conn: Connection,
    sched: Scheduler,
    control: mpsc::UnboundedSender<ServerMessage>,
    /// The client plays audio and hasn't muted it.
    audio: bool,
    view_only: bool,
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
        match (opts.plain, opts.listen) {
            (true, Some(addr)) => endpoint::server_plain(addr, opts.port)?,
            (true, None) => anyhow::bail!("plaintext mode needs an address to listen on"),
            (false, listen) => endpoint::server(listen, opts.port, &opts.identity)?,
        }
    };
    let fingerprint = if opts.plain { "none (plaintext)".to_string() } else { opts.identity.fingerprint() };
    tracing::info!(
        addr = %endpoint.local_addr()?, %fingerprint, rate_mbps = opts.rate_bps / 1_000_000, "listening"
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
    conns: Mutex<Vec<Conn>>,
}

impl Shared {
    /// The speaker encodes while anyone listens.
    fn update_listening(&self, conns: &[Conn]) {
        self.opts.audio.listening.store(conns.iter().any(|c| c.audio), Ordering::Relaxed);
    }
}

async fn run(endpoint: quinn::Endpoint, opts: Options, host: HostSender<ToHost>, rx: mpsc::UnboundedReceiver<ToNet>) {
    let shared = Arc::new(Shared { opts, host, conns: Mutex::default() });
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
            let mut conns = shared.conns.lock().unwrap();
            conns.retain(|c| c.id != id);
            shared.update_listening(&conns);
            let _ = shared.host.send(ToHost::Disconnected(id));
        });
    }
}

/// Routes the host's output to the connections.
async fn dispatch(mut rx: mpsc::UnboundedReceiver<ToNet>, shared: Arc<Shared>) {
    let mut packetizer = Packetizer::new();
    let mut next_update: u32 = 0;
    while let Some(msg) = rx.recv().await {
        let conns = shared.conns.lock().unwrap();
        match msg {
            ToNet::Shutdown(reason) => {
                for c in conns.iter() {
                    c.conn.close(close::SESSION_ENDED.into(), reason.as_bytes());
                }
            }
            ToNet::Frame(f) => {
                // One packetization fits every connection's path.
                let Some(max) = conns.iter().filter_map(|c| c.conn.max_datagram_size()).min() else { continue };
                let frame = EncodedFrame {
                    data: &f.data,
                    keyframe: f.keyframe,
                    epoch: f.epoch,
                    capture_us: f.capture_us,
                    encode_us: f.encode_us,
                };
                let datagrams = packetizer.packetize(&frame, max);
                for c in conns.iter() {
                    c.sched.send_frame(datagrams.clone(), f.keyframe);
                }
            }
            ToNet::Tiles(t) => {
                let count = t.update.bodies.len();
                if count == 0 || conns.is_empty() {
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
                let datagrams: Vec<Bytes> = t
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
                for c in conns.iter() {
                    c.sched.send_frame(datagrams.clone(), false);
                }
            }
            ToNet::Message(id, m) => {
                if let Some(c) = conns.iter().find(|c| c.id == id) {
                    let _ = c.control.send(m);
                }
            }
            ToNet::Broadcast(m) => {
                for c in conns.iter() {
                    let _ = c.control.send(m.clone());
                }
            }
            ToNet::Audio(d) => {
                for c in conns.iter().filter(|c| c.audio) {
                    c.sched.send(Priority::Audio, d.clone());
                }
            }
            ToNet::FetchClipboard(id, paste) => {
                let Some(c) = conns.iter().find(|c| c.id == id) else { continue };
                tokio::spawn(fetch_clipboard(c.conn.clone(), paste));
            }
        }
    }
}

async fn serve(id: ConnId, incoming: quinn::Incoming, shared: &Shared) -> anyhow::Result<()> {
    let (host, audio) = (&shared.host, &shared.opts.audio);
    let (authorized_keys, rate_bps, start) = (&shared.opts.authorized_keys, shared.opts.rate_bps, shared.opts.start);
    let conn = incoming.await.context("handshake")?;
    tracing::info!(id, addr = %conn.remote_address(), "client connected");
    let (mut send, mut recv) = conn.accept_bi().await.context("waiting for the control stream")?;
    let Some(ClientMessage::Hello(hello)) = stream::recv(&mut recv).await? else {
        anyhow::bail!("the client did not start with Hello");
    };
    let key = hello.auth.key;
    let refuse = |why: &str| {
        conn.close(close::UNAUTHORIZED.into(), why.as_bytes());
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
    let view_only = hello.view_only;
    tracing::info!(id, layout = ?hello.layout, decoders = hello.decoders.len(), view_only, "hello");

    let sched = Scheduler::spawn(conn.clone(), rate_bps, endpoint::DATAGRAM_BUFFER);
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    // Audio goes to a client that plays it, from a session that has it.
    let audio_config = hello.audio.and(audio.config.get().copied());
    let next =
        Conn { id, conn: conn.clone(), sched: sched.clone(), control: control_tx, audio: audio_config.is_some(), view_only };
    {
        let mut conns = shared.conns.lock().unwrap();
        if !view_only {
            for prev in conns.iter().filter(|c| !c.view_only) {
                tracing::info!(id, previous = prev.id, "taking over the session");
                prev.conn.close(close::TAKEN_OVER.into(), b"another client took over");
            }
            conns.retain(|c| c.view_only);
        }
        conns.push(next);
        shared.update_listening(&conns);
        // Under the lock, so the host hears of connections in the order
        // they took over.
        let _ = host.send(ToHost::Connected(id, hello));
    }

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
                let mut conns = shared.conns.lock().unwrap();
                if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
                    c.audio = play && audio_config.is_some();
                    tracing::info!(id, play, "audio");
                }
                shared.update_listening(&conns);
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
    // Each clipboard fetch from the client is a stream of its own.
    let streams = async {
        loop {
            let (mut send, mut recv) = conn.accept_bi().await?;
            let host = host.clone();
            tokio::spawn(async move {
                let Ok(Some(request)) = stream::recv::<ClipboardRequest>(&mut recv).await else { return };
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = host.send(ToHost::ClipboardRead(id, request, tx));
                match rx.await {
                    Ok(Some(data)) => {
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                    _ => {
                        let _ = send.reset(0u32.into());
                    }
                }
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    tokio::select! {
        r = writer => r,
        r = reader => r,
        r = datagrams => r,
        r = streams => r,
    }
}

/// Fetches the client's clipboard for a paste in the session, and writes
/// it into the paste's pipe.
async fn fetch_clipboard(conn: Connection, paste: crate::clipboard::Send) {
    let result = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        stream::send(&mut send, &ClipboardRequest { serial: paste.serial, mime: paste.mime.clone() }).await?;
        send.finish()?;
        anyhow::Ok(recv.read_to_end(MAX_CLIPBOARD).await?)
    }
    .await;
    match result {
        Ok(data) => {
            // A pipe write may block until the app reads it.
            let _ = tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let _ = std::fs::File::from(paste.fd).write_all(&data);
            })
            .await;
        }
        Err(err) => tracing::debug!(mime = paste.mime, "fetching the client's clipboard: {err:#}"),
    }
}
