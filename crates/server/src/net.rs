//! The server's network side: a tokio thread that owns the QUIC endpoint
//! and talks to the host's event loop through two channels.
//!
//! One client controls the session at a time: a new controlling client
//! takes over, and the previous one is closed. Any number of others may
//! watch, view-only (§6). Video, tiles, cursor and audio go to every
//! connection alike, encoded once; replies go to the connection they answer.
//! The session itself outlives them all.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::{self, EncodedFrame};
use farsight_net::cc;
use farsight_net::path::{self, PathState};
use farsight_net::quinn::{self, Connection};
use farsight_net::sched::{Priority, Scheduler};
use farsight_net::{auth, endpoint, stream};
use farsight_proto::audio::AudioConfig;
use farsight_proto::control::{ClientMessage, ClipboardRequest, Hello, MAX_CLIPBOARD, ServerMessage, close};
use farsight_proto::datagram::{self, Datagram, Nack, Pong};
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
    /// The audio format changed, for every client that plays audio.
    AudioConfig(AudioConfig),
    /// An app in the session started or stopped recording, for the client
    /// that would send its microphone.
    MicDemand(bool),
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
    /// Its number, and the newest frame it references (RFI, §2).
    pub number: u32,
    pub refs: u32,
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
pub struct Audio {
    /// The stream's format: set once the speaker is up, and again when it
    /// changes.
    pub config: Mutex<Option<AudioConfig>>,
    /// A client is listening: the speaker encodes and sends.
    pub listening: AtomicBool,
    /// Frames per datagram, for the lossiest listener's path (§8).
    pub redundancy: AtomicUsize,
    /// The slowest listener's path is slow: 10 ms frames, at a lower
    /// bitrate.
    pub slow: AtomicBool,
    /// The session's microphone.
    pub mic: crate::mic::Mic,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            config: Mutex::default(),
            listening: AtomicBool::new(false),
            redundancy: AtomicUsize::new(farsight_proto::audio::REDUNDANCY),
            slow: AtomicBool::new(false),
            mic: crate::mic::Mic::default(),
        }
    }
}

/// Audio goes to 10 ms frames when a listener's path is slower than this,
/// and back when they all are faster than the second.
const AUDIO_SLOW: u64 = 1_500_000;
const AUDIO_FAST: u64 = 3_000_000;

/// The fewest frames per datagram that leave a frame lost (as many
/// datagrams lost in a row) under one in ten thousand, at packet loss
/// rate `loss`.
fn redundancy(loss: f64) -> usize {
    use farsight_proto::audio::{MAX_REDUNDANCY, REDUNDANCY};
    (REDUNDANCY..MAX_REDUNDANCY).find(|&d| loss.powi(d as i32) <= 1e-4).unwrap_or(MAX_REDUNDANCY)
}

pub struct Options {
    pub listen: Option<std::net::IpAddr>,
    /// Plaintext mode (§1); needs `listen`.
    pub plain: bool,
    pub port: u16,
    /// Client keys allowed in (§6), read on every connection.
    pub authorized_keys: std::path::PathBuf,
    pub identity: endpoint::Identity,
    /// The most video is paced at; congestion control picks the rate
    /// below it, per connection (§1).
    pub rate_bps: u64,
    /// What the encoder should aim for, in bits per second: set from the
    /// slowest connection's rate.
    pub video_target: Arc<AtomicU64>,
    /// When the video queued for the slowest connection will have gone, in
    /// the host's µs: the pipeline encodes nothing new until about then.
    pub video_drain_at: Arc<AtomicU64>,
    /// The host's clock, which pongs and frame timestamps are in.
    pub start: Instant,
    pub audio: Arc<Audio>,
}

/// A client that said hello.
struct Conn {
    id: ConnId,
    conn: Connection,
    sched: Scheduler,
    path: Arc<PathState>,
    control: mpsc::UnboundedSender<ServerMessage>,
    /// The client plays audio and hasn't muted it.
    audio: bool,
    /// The client plays audio, muted or not.
    plays: bool,
    /// The client controls the session and has a microphone.
    mic: bool,
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
        addr = %endpoint.local_addr()?, %fingerprint, max_rate_mbps = opts.rate_bps / 1_000_000, "listening"
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
    /// The last frames sent, for NACKs: number, data shards, datagrams.
    history: Mutex<VecDeque<(u32, u16, Vec<Bytes>)>>,
}

/// Frames kept for NACKs. They come only on paths quicker than a frame,
/// so this is plenty.
const HISTORY: usize = 32;

impl Shared {
    /// The speaker encodes while anyone listens.
    fn update_listening(&self, conns: &[Conn]) {
        self.opts.audio.listening.store(conns.iter().any(|c| c.audio), Ordering::Relaxed);
    }

    /// The shards a NACK asks for, as far as they are still at hand.
    fn repair(&self, nack: &Nack) -> Vec<Bytes> {
        let history = self.history.lock().unwrap();
        let Some((_, data, datagrams)) = history.iter().find(|(n, _, _)| *n == nack.frame) else {
            return Vec::new();
        };
        match nack.shards.is_empty() {
            // A frame the client has none of: its data shards.
            true => datagrams[..*data as usize].to_vec(),
            false => nack.shards.iter().filter_map(|&i| datagrams.get(i as usize).cloned()).collect(),
        }
    }

    /// Notes when the slowest connection's video queue will have drained.
    fn note_backlog(&self, conns: &[Conn]) {
        let backlog = conns.iter().map(|c| c.sched.backlog()).max().unwrap_or_default();
        let at = self.opts.start.elapsed() + backlog;
        self.opts.video_drain_at.store(at.as_micros() as u64, Ordering::Relaxed);
    }

    /// Video and audio are encoded once for everyone, so for the slowest
    /// path, and the lossiest.
    fn update_paths(&self) {
        let conns = self.conns.lock().unwrap();
        if let Some(target) = conns.iter().map(|c| encoder_target(&c.path)).min() {
            self.opts.video_target.store(target, Ordering::Relaxed);
        }
        let audio = &self.opts.audio;
        let listeners = || conns.iter().filter(|c| c.audio).map(|c| &c.path);
        let loss = listeners().map(|p| p.loss()).fold(0.0, f64::max);
        audio.redundancy.store(redundancy(loss), Ordering::Relaxed);
        if let Some(rate) = listeners().map(|p| p.rate_bps()).min() {
            let slow = audio.slow.load(Ordering::Relaxed);
            if !slow && rate < AUDIO_SLOW || slow && rate > AUDIO_FAST {
                audio.slow.store(!slow, Ordering::Relaxed);
            }
        }
    }
}

/// Audio's share of a path, when it plays: Opus and the packets around it.
const AUDIO_RESERVE: u64 = 300_000;

/// The share of a path's rate the encoder aims for. The rest leaves room
/// for FEC, audio, input, and frames bigger than the rest.
const ENCODER_SHARE: f64 = 0.85;

/// The encoder's target for a path: its share, less parity for the loss
/// (about twice the loss, as [`farsight_net::packetize::parity`] sizes it,
/// and a little more for small frames) and audio.
fn encoder_target(p: &PathState) -> u64 {
    let fec = 1.0 + 3.0 * p.loss();
    ((p.rate_bps() as f64 * ENCODER_SHARE / fec) as u64).saturating_sub(AUDIO_RESERVE).max(cc::MIN_RATE / 2)
}

async fn run(endpoint: quinn::Endpoint, opts: Options, host: HostSender<ToHost>, rx: mpsc::UnboundedReceiver<ToNet>) {
    let shared = Arc::new(Shared { opts, host, conns: Mutex::default(), history: Mutex::default() });
    tokio::spawn(dispatch(rx, shared.clone()));
    tokio::spawn(report_mic(shared.clone()));
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
/// Video sent since the last log line: frames, their bytes, the datagrams'
/// bytes (headers, padding and parity on top), and the longest
/// packetization (parity takes the time).
#[derive(Default)]
struct VideoSent {
    frames: u32,
    bytes: usize,
    sent: usize,
    longest: std::time::Duration,
}

/// Video sent is logged this often.
const VIDEO_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

async fn dispatch(mut rx: mpsc::UnboundedReceiver<ToNet>, shared: Arc<Shared>) {
    let mut next_update: u32 = 0;
    let (mut video, mut logged) = (VideoSent::default(), Instant::now());
    while let Some(msg) = rx.recv().await {
        if logged.elapsed() >= VIDEO_LOG_EVERY && video.frames > 0 {
            let secs = logged.elapsed().as_secs_f64();
            tracing::info!(
                fps = format!("{:.1}", video.frames as f64 / secs),
                mbps = format!("{:.2}", video.sent as f64 * 8.0 / secs / 1e6),
                overhead = format!("{:.0}%", (video.sent as f64 / video.bytes.max(1) as f64 - 1.0) * 100.0),
                packetize_max_us = video.longest.as_micros() as u64,
                "video sent"
            );
            (video, logged) = (VideoSent::default(), Instant::now());
        }
        let conns = shared.conns.lock().unwrap();
        match msg {
            ToNet::Shutdown(reason) => {
                for c in conns.iter() {
                    c.conn.close(close::SESSION_ENDED.into(), reason.as_bytes());
                }
            }
            ToNet::Frame(f) => {
                // One packetization fits every connection's path, and
                // its parity the worst loss among them.
                let Some(max) = conns.iter().filter_map(|c| c.conn.max_datagram_size()).min() else { continue };
                let loss = conns.iter().map(|c| c.path.loss()).fold(0.0, f64::max);
                let frame = EncodedFrame {
                    data: &f.data,
                    keyframe: f.keyframe,
                    epoch: f.epoch,
                    frame: f.number,
                    refs: f.refs,
                    capture_us: f.capture_us,
                    encode_us: f.encode_us,
                };
                let started = Instant::now();
                let datagrams = packetize::packetize(&frame, max, loss);
                video.longest = video.longest.max(started.elapsed());
                video.frames += 1;
                video.bytes += f.data.len();
                video.sent += datagrams.iter().map(Bytes::len).sum::<usize>();
                for c in conns.iter() {
                    c.sched.send_frame(datagrams.clone(), f.keyframe);
                }
                shared.note_backlog(&conns);
                let data = match Datagram::decode(&datagrams[0]) {
                    Some(Datagram::Video(h, _)) => h.data,
                    _ => 0,
                };
                let mut history = shared.history.lock().unwrap();
                if history.len() == HISTORY {
                    history.pop_front();
                }
                history.push_back((f.number, data, datagrams));
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
                shared.note_backlog(&conns);
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
            ToNet::AudioConfig(config) => {
                for c in conns.iter().filter(|c| c.plays) {
                    let _ = c.control.send(ServerMessage::AudioConfig(config));
                }
            }
            ToNet::MicDemand(on) => {
                for c in conns.iter().filter(|c| c.mic) {
                    let _ = c.control.send(ServerMessage::MicDemand(on));
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

async fn serve(id: ConnId, incoming: quinn::Incoming, shared: &Arc<Shared>) -> anyhow::Result<()> {
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

    let sched = Scheduler::spawn(conn.clone(), cc::START_RATE.min(rate_bps), endpoint::DATAGRAM_BUFFER);
    // Video is paced at the path's rate, less audio's share.
    let path = {
        let (sched, shared) = (sched.clone(), shared.clone());
        path::monitor(conn.clone(), rate_bps, move |p| {
            sched.set_rate(p.rate_bps().saturating_sub(AUDIO_RESERVE).max(cc::MIN_RATE / 2));
            shared.update_paths();
        })
    };
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    // Audio goes to a client that plays it, from a session that has it.
    let plays = hello.audio.is_some() && audio.config.lock().unwrap().is_some();
    let mic = hello.mic && !view_only;
    let next = Conn {
        id,
        conn: conn.clone(),
        sched: sched.clone(),
        path,
        control: control_tx,
        audio: plays,
        plays,
        mic,
        view_only,
    };
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
            // The audio format follows the welcome, which comes first, and
            // so does a recording already under way.
            if let ServerMessage::Welcome(_) = msg {
                let config = *audio.config.lock().unwrap();
                if let (Some(config), true) = (config, plays) {
                    stream::send(&mut send, &ServerMessage::AudioConfig(config)).await?;
                }
                if mic && audio.mic.demand() {
                    stream::send(&mut send, &ServerMessage::MicDemand(true)).await?;
                }
            }
        }
        anyhow::Ok(())
    };
    let reader = async {
        while let Some(msg) = stream::recv::<ClientMessage>(&mut recv).await? {
            if let ClientMessage::SetAudio { play } = msg {
                let mut conns = shared.conns.lock().unwrap();
                if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
                    c.audio = play && plays;
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
                Some(Datagram::Mic(p)) if mic => audio.mic.push(&p, start.elapsed().as_micros() as u64),
                Some(Datagram::Ping(p)) => {
                    let pong = Pong { client_us: p.client_us, server_us: start.elapsed().as_micros() as u64 };
                    sched.send(Priority::Input, Bytes::from(Datagram::Pong(pong).to_vec()));
                }
                // A frame dropped here went for want of room.
                Some(Datagram::Nack(nack)) if !sched.was_dropped(nack.frame) => {
                    let repair = shared.repair(&nack);
                    tracing::debug!(id, frame = nack.frame, asked = nack.shards.len(), sent = repair.len(), "NACK");
                    sched.send_repair(repair);
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

/// Logs the microphone's latency and the buffer's health every few
/// seconds, while an app records.
async fn report_mic(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(VIDEO_LOG_EVERY);
    loop {
        tick.tick().await;
        let Some(mut s) = shared.opts.audio.mic.take_stats() else { continue };
        if s.latency_us.is_empty() {
            continue;
        }
        s.latency_us.sort_unstable();
        let pct = |p: usize| s.latency_us[(s.latency_us.len() * p / 100).min(s.latency_us.len() - 1)] as f64 / 1000.0;
        tracing::info!(
            "microphone: capture→source ms p50 {:.1} p95 {:.1}; buffer {:.1} ms (target {:.1}); speed {:+.3}%; concealed {} (fec {}, late {}) underruns {} skipped {}",
            pct(50),
            pct(95),
            s.buffered_us as f64 / 1000.0,
            s.target_us as f64 / 1000.0,
            s.adjust * 100.0,
            s.concealed,
            s.fec,
            s.late,
            s.underruns,
            s.skipped,
        );
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

#[cfg(test)]
mod tests {
    #[test]
    fn audio_redundancy_follows_loss() {
        assert_eq!(super::redundancy(0.0), 3);
        assert_eq!(super::redundancy(0.05), 4);
        assert_eq!(super::redundancy(0.1), 5);
        assert_eq!(super::redundancy(0.2), 6);
    }
}
