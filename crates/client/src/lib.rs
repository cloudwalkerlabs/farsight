//! Platform-independent client core: connection, codec negotiation, input
//! state sync and layout reporting. The desktop and Android apps wrap it.
//!
//! [`Client::connect`] runs on a tokio runtime and reports to the app
//! through a callback: video epochs, whole frames ready to decode, cursor
//! changes and the end of the connection. Frames are rebuilt from parity
//! where they can be, asked for again on a quick path (NACK), and handed
//! over in order, and only those whose references the decoder has: after
//! a loss the core drops frames and asks the server to predict from the
//! last good one (RFI) or for a keyframe, so the decoder never sees a
//! damaged stream (§2). Each epoch's `Epoch` event comes before its first
//! frame, even when the frame overtook the announcement on the wire.
//!
//! Tiles (when the server has no hardware encoder) are handed over a
//! datagram at a time, as each decodes on its own. When part of an update
//! is lost, the core asks for the cells it is missing.
//!
//! Audio goes into a jitter buffer here ([`audio::Player`]); the app opens
//! its output when `AudioConfig` arrives and pulls samples through
//! [`Client::fill_audio`].

pub mod audio;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::{Reassembler, Repair};
use farsight_net::quinn::Connection;
use farsight_net::{endpoint, quinn, stream};
use farsight_proto::audio::{AudioCaps, AudioConfig};
use farsight_proto::codec::{DecoderCaps, Format, Mode};
use farsight_net::auth::{ClientKey, KnownHosts, Pin};
use farsight_proto::control::{
    ClientAuth, ClientMessage, ClipboardOffer, ClipboardRequest, CursorImage, CursorShape, Epoch, Hello,
    MAX_CLIPBOARD, ServerMessage, Welcome, close,
};
use farsight_proto::datagram::{Datagram, Ping};
use farsight_proto::input::{InputEvent, InputPacket, InputSender};
use farsight_proto::layout::Layout;
use farsight_proto::tiles::{self, Rect, TilesHeader};
use farsight_proto::video::{FragmentHeader, before};
use tokio::sync::mpsc;

/// A frame missing fragments this long after its first one is lost. Long
/// enough for a paced keyframe to arrive.
const FRAME_TIMEOUT_US: u64 = 250_000;

/// An RFI or keyframe request is repeated this long, plus two round trips,
/// after the last until the decoder can go on.
const ASK_RETRY: Duration = Duration::from_millis(50);

/// Missing shards are asked for again (NACK) when the answer can come
/// within a frame: a round trip, and a couple of shards' time on the path.
const NACK_MAX_RTT_US: u64 = 16_000;

/// A NACK's answer is given this long beyond a round trip.
const NACK_SLACK_US: u64 = 4_000;

/// How often the clock offset to the server is measured.
const PING_INTERVAL: Duration = Duration::from_millis(500);

/// Clock samples kept; the one with the shortest round trip wins.
const PING_SAMPLES: usize = 16;

/// Frames held while their epoch's announcement is on its way.
const MAX_HELD: usize = 64;

/// A connection the server or the user's records refused: connecting
/// again won't help until something changes (a key, a pin).
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// The client core's version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub struct Config {
    pub addr: SocketAddr,
    /// The server's name as the user gave it, `host:port`: its key in
    /// `known_hosts`.
    pub server_name: String,
    pub key: Arc<ClientKey>,
    pub known_hosts: KnownHosts,
    /// Plaintext mode (§1): no TLS, for tailnets.
    pub plain: bool,
    pub layout: Layout,
    pub decoders: Vec<DecoderCaps>,
    pub mode: Mode,
    /// `None`: no audio.
    pub audio: Option<AudioCaps>,
    /// Watch only, next to the client in control (§6).
    pub view_only: bool,
}

#[derive(Debug)]
pub enum Event {
    Connected { fingerprint: String, welcome: Welcome },
    /// A new epoch: the frames after this are in its encoding and layout.
    Epoch(Epoch),
    /// A whole frame, ready to decode.
    Frame(VideoFrame),
    /// One datagram of tiles, ready to decode.
    Tiles(TilesPacket),
    CursorImage(CursorImage),
    Cursor(CursorShape),
    /// The session's audio starts, in this format: open the output and
    /// call [`Client::fill_audio`] from it.
    AudioConfig(AudioConfig),
    /// The session's clipboard changed; fetch what is needed with
    /// [`Client::fetch_clipboard`].
    ClipboardOffer(ClipboardOffer),
    /// A text field in the session gained (true) or lost focus.
    TextInput(bool),
    /// The connection ended. `retry`: it was lost rather than ended, so
    /// connecting again resumes the session.
    Closed { reason: String, retry: bool },
}

#[derive(Debug)]
pub struct VideoFrame {
    pub header: FragmentHeader,
    pub data: Vec<u8>,
    /// When its first and last fragments arrived, in [`Client::now_us`].
    pub first_us: u64,
    pub complete_us: u64,
}

#[derive(Debug)]
pub struct TilesPacket {
    pub header: TilesHeader,
    pub body: Vec<u8>,
    /// When it arrived, in [`Client::now_us`].
    pub received_us: u64,
}

/// Running counts, for the app's statistics.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub lost: u64,
    /// Frames rebuilt from parity (FEC), and completed by shards asked for
    /// again (NACK).
    pub recovered: u64,
    pub repaired: u64,
    /// Asked to predict from the last good frame (RFI), and for keyframes.
    pub rfi_requests: u64,
    pub keyframe_requests: u64,
    /// The shortest recent ping round trip, in µs.
    pub rtt_us: u64,
    /// The longest round trip since the last call to [`Client::stats`].
    pub rtt_max_us: u64,
}

/// A connected client. Methods may be called from any thread.
pub struct Client {
    conn: Connection,
    shared: Arc<Shared>,
    control: mpsc::UnboundedSender<ClientMessage>,
}

struct Shared {
    start: Instant,
    input: Mutex<InputSender>,
    /// Server clock minus client clock, in µs, once measured.
    offset_us: AtomicI64,
    offset_known: AtomicBool,
    rtt_us: AtomicU64,
    rtt_max_us: AtomicU64,
    /// The decoder lost its state: only a keyframe will do.
    reset_chain: AtomicBool,
    stats: Mutex<Stats>,
    /// Set once the server announces its audio.
    audio: Mutex<Option<audio::Player>>,
    /// The client's clipboard as offered to the server: its serial and
    /// what gives the data for a MIME type.
    clipboard: Mutex<Option<(u32, ClipboardProvider)>>,
}

/// Gives the client's clipboard data in a MIME type, when an app in the
/// session pastes.
pub type ClipboardProvider = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

impl Shared {
    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

type Callback = Arc<dyn Fn(Event) + Send + Sync>;

impl Client {
    /// Connects, says hello and waits for the server's welcome. Must be
    /// called on a tokio runtime; the connection's tasks run there and
    /// report through `on_event`, starting with `Connected`.
    pub async fn connect(cfg: Config, on_event: impl Fn(Event) + Send + Sync + 'static) -> anyhow::Result<Self> {
        let on_event: Callback = Arc::new(on_event);
        let endpoint = endpoint::client(cfg.plain)?;
        // Never TLS then plaintext: the mode is the user's, and known_hosts
        // refuses a downgrade before anything is sent.
        let (conn, fingerprint) = endpoint::connect(&endpoint, cfg.addr).await?;
        let pin = match (cfg.plain, &fingerprint) {
            (true, _) => Pin::Plain,
            (false, Some(fp)) => Pin::Tls(fp.clone()),
            (false, None) => anyhow::bail!("the server sent no certificate"),
        };
        let fingerprint = fingerprint.unwrap_or_else(|| "none (plaintext)".into());
        tracing::info!(addr = %cfg.addr, %fingerprint, "connected");
        // Nothing goes to a server that isn't the one we know.
        if let Err(err) = cfg.known_hosts.check(&cfg.server_name, &pin) {
            conn.close(0u32.into(), b"unknown server");
            return Err(Refused(format!("{err:#}")).into());
        }
        let (mut send, mut recv) = conn.open_bi().await.context("opening the control stream")?;
        let auth = ClientAuth { key: cfg.key.public(), signature: cfg.key.sign(&conn)? };
        let hello = Hello {
            decoders: cfg.decoders,
            layout: cfg.layout,
            mode: cfg.mode,
            audio: cfg.audio,
            auth,
            view_only: cfg.view_only,
        };
        stream::send(&mut send, &ClientMessage::Hello(hello)).await?;
        let welcome = match stream::recv(&mut recv).await {
            Ok(m) => m,
            Err(_) if unauthorized(&conn) => {
                return Err(Refused(format!(
                    "the server doesn't know this client's key. On the server, add this line to its authorized_keys:\n  {}",
                    cfg.key.authorized_line("")
                ))
                .into());
            }
            Err(err) => return Err(err),
        };
        let welcome = match welcome {
            Some(ServerMessage::Welcome(w)) => w,
            Some(other) => anyhow::bail!("expected Welcome, got {other:?}"),
            None => anyhow::bail!("the server closed the control stream"),
        };
        tracing::info!(encodings = ?welcome.encodings.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "welcome");

        let shared = Arc::new(Shared {
            start: Instant::now(),
            input: Mutex::new(InputSender::new()),
            offset_us: AtomicI64::new(0),
            offset_known: AtomicBool::new(false),
            rtt_us: AtomicU64::new(0),
            rtt_max_us: AtomicU64::new(0),
            reset_chain: AtomicBool::new(false),
            stats: Mutex::default(),
            audio: Mutex::default(),
            clipboard: Mutex::default(),
        });
        let (control, control_rx) = mpsc::unbounded_channel();
        on_event(Event::Connected { fingerprint, welcome });

        tokio::spawn(run(conn.clone(), shared.clone(), send, recv, (control.clone(), control_rx), on_event, endpoint));
        Ok(Self { conn, shared, control })
    }

    /// Sends one input event at once, with recent history and, while
    /// anything is held, a snapshot.
    pub fn input(&self, event: InputEvent) {
        let packet = self.shared.input.lock().unwrap().push(event, self.now_us() / 1000);
        self.send_input(packet);
    }

    /// Tells the server nothing is held any more, as when the window loses
    /// focus.
    pub fn release_all(&self) {
        let packet = self.shared.input.lock().unwrap().release_all(self.now_us() / 1000);
        self.send_input(packet);
    }

    /// The decoder failed: drop frames until the next keyframe.
    pub fn request_keyframe(&self) {
        self.shared.reset_chain.store(true, Ordering::Relaxed);
    }

    /// The window's size or scale changed (§5).
    pub fn set_layout(&self, layout: Layout) {
        let _ = self.control.send(ClientMessage::SetLayout(layout));
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.control.send(ClientMessage::SetMode(mode));
    }

    /// The decoder for `format` can't be used; the server moves on to its
    /// next choice, in a new epoch.
    pub fn decoder_failed(&self, format: Format) {
        self.shared.reset_chain.store(true, Ordering::Relaxed);
        let _ = self.control.send(ClientMessage::DecoderFailed(format));
    }

    /// The client's clock, in µs since connecting.
    pub fn now_us(&self) -> u64 {
        self.shared.now_us()
    }

    /// A server timestamp on the client's clock, once the offset is known.
    pub fn server_to_local(&self, server_us: u64) -> Option<u64> {
        self.shared
            .offset_known
            .load(Ordering::Relaxed)
            .then(|| (server_us as i64 - self.shared.offset_us.load(Ordering::Relaxed)) as u64)
    }

    pub fn stats(&self) -> Stats {
        let mut s = *self.shared.stats.lock().unwrap();
        s.rtt_us = self.shared.rtt_us.load(Ordering::Relaxed);
        s.rtt_max_us = self.shared.rtt_max_us.swap(0, Ordering::Relaxed);
        s
    }

    /// Fills `out` (interleaved, in the `AudioConfig`'s format) with audio
    /// to be heard `output_delay_us` from now: the output's own latency.
    /// Silence until there is audio.
    pub fn fill_audio(&self, out: &mut [f32], output_delay_us: u64) {
        let now_us = self.now_us();
        let offset = self.shared.offset_known.load(Ordering::Relaxed).then(|| self.shared.offset_us.load(Ordering::Relaxed));
        match self.shared.audio.lock().unwrap().as_mut() {
            Some(player) => player.pull(out, now_us, output_delay_us, offset),
            None => out.fill(0.0),
        }
    }

    /// Audio statistics since the last call, if there is audio.
    pub fn audio_stats(&self) -> Option<audio::AudioStats> {
        self.shared.audio.lock().unwrap().as_mut().map(|p| p.take_stats())
    }

    /// The client's clipboard changed: offer its MIME types to the session.
    /// `provider` gives the data when an app there pastes.
    pub fn offer_clipboard(&self, mimes: Vec<String>, provider: ClipboardProvider) {
        let mut slot = self.shared.clipboard.lock().unwrap();
        let serial = slot.as_ref().map_or(1, |(s, _)| s.wrapping_add(1));
        *slot = Some((serial, provider));
        let _ = self.control.send(ClientMessage::ClipboardOffer(ClipboardOffer { serial, mimes }));
    }

    /// Fetches the session's clipboard, offer `serial`, as `mime`. Fails
    /// if the selection has changed since.
    pub async fn fetch_clipboard(&self, serial: u32, mime: &str) -> anyhow::Result<Vec<u8>> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        stream::send(&mut send, &ClipboardRequest { serial, mime: mime.to_string() }).await?;
        send.finish()?;
        Ok(recv.read_to_end(MAX_CLIPBOARD).await?)
    }

    /// Commits text to the focused field in the session.
    pub fn commit_text(&self, text: String) {
        let _ = self.control.send(ClientMessage::Text(text));
    }

    /// Text being composed; empty clears it.
    pub fn preedit(&self, text: String, cursor: Option<(u32, u32)>) {
        let _ = self.control.send(ClientMessage::Preedit { text, cursor });
    }

    /// Mutes or unmutes the session's audio at the server.
    pub fn set_audio(&self, play: bool) {
        let _ = self.control.send(ClientMessage::SetAudio { play });
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }

    fn send_input(&self, packet: InputPacket) {
        let _ = self.conn.send_datagram(Bytes::from(Datagram::Input(packet).to_vec()));
    }
}

async fn run(
    conn: Connection,
    shared: Arc<Shared>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    (control, mut control_rx): (mpsc::UnboundedSender<ClientMessage>, mpsc::UnboundedReceiver<ClientMessage>),
    on_event: Callback,
    // Kept alive for as long as the connection.
    _endpoint: quinn::Endpoint,
) {
    let writer = async {
        while let Some(msg) = control_rx.recv().await {
            stream::send(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    };
    let gate = Mutex::new(EpochGate::default());
    let reader = async {
        while let Some(msg) = stream::recv::<ServerMessage>(&mut recv).await? {
            match msg {
                ServerMessage::Epoch(epoch) => {
                    tracing::info!(epoch = epoch.epoch, encoding = %epoch.encoding, layout = ?epoch.layout, "epoch");
                    let held = gate.lock().unwrap().announce(epoch.epoch);
                    on_event(Event::Epoch(epoch));
                    for m in held {
                        on_event(m.into_event());
                    }
                }
                ServerMessage::CursorImage(image) => on_event(Event::CursorImage(image)),
                ServerMessage::Cursor(shape) => on_event(Event::Cursor(shape)),
                ServerMessage::Welcome(_) => tracing::warn!("unexpected Welcome"),
                ServerMessage::ClipboardOffer(offer) => on_event(Event::ClipboardOffer(offer)),
                ServerMessage::TextInput(active) => on_event(Event::TextInput(active)),
                ServerMessage::AudioConfig(config) => {
                    tracing::info!(?config, "audio");
                    match audio::Player::new(config) {
                        Ok(player) => {
                            // Another frame size on the same output (a slow
                            // path) needs only a new player.
                            let same_output = shared.audio.lock().unwrap().replace(player).is_some_and(|p| {
                                let old = p.config();
                                (old.channels, old.sample_rate) == (config.channels, config.sample_rate)
                            });
                            if !same_output {
                                on_event(Event::AudioConfig(config));
                            }
                        }
                        Err(err) => tracing::warn!("audio: {err:#}"),
                    }
                }
            }
        }
        anyhow::Ok(())
    };
    let ticks = async {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut next_ping = Instant::now();
        let mut pings = 0u32;
        loop {
            tick.tick().await;
            let now_us = shared.now_us();
            if let Some(p) = shared.input.lock().unwrap().tick(now_us / 1000) {
                let _ = conn.send_datagram(Bytes::from(Datagram::Input(p).to_vec()));
            }
            // A quick burst first, so the offset is known early.
            if Instant::now() >= next_ping {
                let _ = conn.send_datagram(Bytes::from(Datagram::Ping(Ping { client_us: now_us }).to_vec()));
                pings += 1;
                next_ping = Instant::now() + if pings < 8 { Duration::from_millis(40) } else { PING_INTERVAL };
            }
        }
    };
    let datagrams = async {
        let mut rx = Reassembler::new(FRAME_TIMEOUT_US);
        let mut chain = Chain::default();
        let mut tracker = TileTracker::default();
        let mut samples: Vec<(u64, i64)> = Vec::new();
        let err = loop {
            // Quick ticks while a frame is waiting on shards: NACKs and
            // giving up are a matter of milliseconds.
            let wait = Duration::from_millis(if rx.pending() { 2 } else { 50 });
            let d = match tokio::time::timeout(wait, conn.read_datagram()).await {
                Ok(Ok(d)) => Some(d),
                Ok(Err(err)) => break err,
                Err(_) => None,
            };
            let now = shared.now_us();
            match d.as_deref().and_then(Datagram::decode) {
                Some(Datagram::Video(h, payload)) => rx.push(h, payload, now),
                Some(Datagram::Tiles(header, body)) => {
                    tracker.receive(&header, body, now);
                    let packet = TilesPacket { header, body: body.to_vec(), received_us: now };
                    if let Some(m) = gate.lock().unwrap().admit(Media::Tiles(packet)) {
                        on_event(m.into_event());
                    }
                }
                Some(Datagram::Audio(p)) => {
                    if let Some(player) = shared.audio.lock().unwrap().as_mut() {
                        player.push(&p, now);
                    }
                }
                Some(Datagram::Pong(p)) => {
                    let rtt = now.saturating_sub(p.client_us);
                    shared.rtt_max_us.fetch_max(rtt, Ordering::Relaxed);
                    let offset = p.server_us as i64 - (p.client_us + rtt / 2) as i64;
                    if samples.len() == PING_SAMPLES {
                        samples.remove(0);
                    }
                    samples.push((rtt, offset));
                    let &(rtt, offset) = samples.iter().min_by_key(|s| s.0).unwrap();
                    shared.rtt_us.store(rtt, Ordering::Relaxed);
                    shared.offset_us.store(offset, Ordering::Relaxed);
                    shared.offset_known.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
            let refresh = tracker.expire(now);
            if !refresh.is_empty() {
                shared.stats.lock().unwrap().lost += 1;
                tracing::debug!(?refresh, "tiles lost; asking again");
                let _ = control.send(ClientMessage::RequestRefresh(refresh));
            }

            // Video: frames in order, NACKs on a quick path.
            let rtt = shared.offset_known.load(Ordering::Relaxed).then(|| shared.rtt_us.load(Ordering::Relaxed));
            let gap = rx.shard_gap_us();
            let repair = match rtt {
                Some(rtt) if rtt + 2 * gap < NACK_MAX_RTT_US => {
                    Repair { nack: true, wait_us: 2 * rtt + 4 * gap + NACK_SLACK_US }
                }
                _ => Repair::NONE,
            };
            let (frames, nacks) = rx.poll(now, repair);
            for nack in nacks {
                let _ = conn.send_datagram(Bytes::from(Datagram::Nack(nack).to_vec()));
            }
            if shared.reset_chain.swap(false, Ordering::Relaxed) {
                chain.reset();
            }
            let lost = rx.take_lost();
            if let Some(lost) = lost {
                tracing::debug!(count = lost.count, newest = lost.newest, "frames lost");
                chain.lost(lost.newest);
            }
            {
                let mut stats = shared.stats.lock().unwrap();
                stats.frames += frames.len() as u64;
                stats.lost += lost.map_or(0, |l| l.count) as u64;
                stats.recovered += rx.take_recovered() as u64;
                stats.repaired += rx.take_repaired() as u64;
            }
            for f in frames {
                if !chain.admit(&f.header) {
                    tracing::trace!(frame = f.header.frame, refs = f.header.refs, "can't decode; dropped");
                    continue;
                }
                let f = VideoFrame { header: f.header, data: f.data, first_us: f.first_us, complete_us: f.complete_us };
                if let Some(m) = gate.lock().unwrap().admit(Media::Frame(f)) {
                    on_event(m.into_event());
                }
            }
            let retry = ASK_RETRY + Duration::from_micros(2 * rtt.unwrap_or(0));
            match chain.ask(Instant::now(), retry) {
                Some(Ask::Rfi { lost, good }) => {
                    tracing::debug!(lost, good, "asking to predict from the last good frame");
                    shared.stats.lock().unwrap().rfi_requests += 1;
                    let _ = control.send(ClientMessage::Rfi { lost, good });
                }
                Some(Ask::Keyframe) => {
                    tracing::debug!("asking for a keyframe");
                    shared.stats.lock().unwrap().keyframe_requests += 1;
                    let _ = control.send(ClientMessage::RequestKeyframe);
                }
                None => {}
            }
        };
        Err::<(), _>(anyhow::Error::from(err))
    };
    // The server fetching the client's clipboard, a stream each time.
    let streams = async {
        loop {
            let (mut send, mut recv) = conn.accept_bi().await?;
            let shared = shared.clone();
            tokio::spawn(async move {
                let Ok(Some(request)) = stream::recv::<ClipboardRequest>(&mut recv).await else { return };
                let provider = shared.clipboard.lock().unwrap().clone();
                let data = match provider {
                    Some((serial, provider)) if serial == request.serial => {
                        tokio::task::spawn_blocking(move || provider(&request.mime)).await.ok().flatten()
                    }
                    _ => None,
                };
                match data {
                    Some(data) => {
                        let _ = send.write_all(&data).await;
                        let _ = send.finish();
                    }
                    None => {
                        let _ = send.reset(0u32.into());
                    }
                }
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let result = tokio::select! {
        r = writer => r,
        r = reader => r,
        r = datagrams => r,
        r = streams => r,
        () = ticks => Ok(()),
    };
    let (reason, retry) = match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(c)) => {
            (String::from_utf8_lossy(&c.reason).into_owned(), false)
        }
        Some(quinn::ConnectionError::LocallyClosed) => ("closed".to_string(), false),
        Some(other) => (other.to_string(), true),
        None => match result {
            Ok(()) => ("the server closed the control stream".to_string(), false),
            Err(err) => (format!("{err:#}"), true),
        },
    };
    tracing::info!(%reason, retry, "disconnected");
    conn.close(0u32.into(), b"");
    on_event(Event::Closed { reason, retry });
}

/// The server closed the connection for want of an authorized key.
fn unauthorized(conn: &Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(quinn::ConnectionError::ApplicationClosed(c)) if c.error_code == close::UNAUTHORIZED.into()
    )
}

/// Which frames the decoder can take (§2): a keyframe, or a frame whose
/// newest reference is no newer than the last frame decoded, back to the
/// keyframe the chain started from. Anything else is dropped, and the
/// server is asked to predict from the last good frame (RFI), or, with no
/// good frame, for a keyframe.
#[derive(Debug, Default)]
struct Chain {
    /// The last frame decoded, and the keyframe its chain started from.
    decoded: Option<(u32, u32)>,
    /// The newest frame the decoder couldn't have.
    broken: Option<u32>,
    asked: Option<Instant>,
}

/// What [`Chain::ask`] wants sent.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    Rfi { lost: u32, good: u32 },
    Keyframe,
}

impl Chain {
    /// Whether frame `h` can be decoded; if so, it is taken as decoded.
    fn admit(&mut self, h: &FragmentHeader) -> bool {
        let ok = h.keyframe()
            || self.decoded.is_some_and(|(last, key)| !before(last, h.refs) && !before(h.refs, key));
        if !ok {
            self.lost(h.frame);
            return false;
        }
        let key = if h.keyframe() { h.frame } else { self.decoded.map_or(h.frame, |(_, k)| k) };
        self.decoded = Some((h.frame, key));
        // Everything lost so far was older: the chain is whole again.
        if self.broken.is_some_and(|b| before(b, h.frame)) {
            self.broken = None;
            self.asked = None;
        }
        true
    }

    /// Frame `n` won't reach the decoder.
    fn lost(&mut self, n: u32) {
        if self.decoded.is_some_and(|(last, _)| !before(last, n)) {
            return; // older than what was decoded since
        }
        self.broken = Some(self.broken.map_or(n, |b| if before(b, n) { n } else { b }));
    }

    /// The decoder lost its state.
    fn reset(&mut self) {
        let newest = self.decoded.map(|(last, _)| last);
        self.decoded = None;
        self.broken = Some(self.broken.or(newest).unwrap_or(0));
        self.asked = None;
    }

    /// What to ask the server for now, at most once per `retry`.
    fn ask(&mut self, now: Instant, retry: Duration) -> Option<Ask> {
        let lost = self.broken?;
        if self.asked.is_some_and(|t| now.duration_since(t) < retry) {
            return None;
        }
        self.asked = Some(now);
        Some(match self.decoded {
            Some((good, _)) => Ask::Rfi { lost, good },
            None => Ask::Keyframe,
        })
    }
}

/// Holds frames back until their epoch is announced, so the app always
/// knows a frame's encoding before it sees the frame.
#[derive(Default)]
struct EpochGate {
    known: Option<u16>,
    held: Vec<Media>,
}

/// What the gate holds.
#[derive(Debug)]
enum Media {
    Frame(VideoFrame),
    Tiles(TilesPacket),
}

impl Media {
    fn epoch(&self) -> u16 {
        match self {
            Media::Frame(f) => f.header.epoch,
            Media::Tiles(t) => t.header.epoch,
        }
    }

    fn into_event(self) -> Event {
        match self {
            Media::Frame(f) => Event::Frame(f),
            Media::Tiles(t) => Event::Tiles(t),
        }
    }
}

/// `a` comes after `b`, across wrap-around.
fn epoch_after(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) > 0
}

impl EpochGate {
    /// The frame, if it can go to the app now. Frames of older epochs are
    /// dropped; frames of newer ones wait.
    fn admit(&mut self, f: Media) -> Option<Media> {
        match self.known {
            Some(e) if e == f.epoch() => Some(f),
            Some(e) if epoch_after(e, f.epoch()) => None,
            _ => {
                if self.held.len() == MAX_HELD {
                    self.held.remove(0);
                }
                self.held.push(f);
                None
            }
        }
    }

    /// Epoch `epoch` starts; returns its frames that were waiting.
    fn announce(&mut self, epoch: u16) -> Vec<Media> {
        self.known = Some(epoch);
        let held = std::mem::take(&mut self.held);
        let (now, later): (Vec<_>, Vec<_>) = held
            .into_iter()
            .filter(|f| !epoch_after(epoch, f.epoch()))
            .partition(|f| f.epoch() == epoch);
        self.held = later;
        now
    }
}

/// Tiles updates still missing datagrams. One that stays incomplete for
/// [`FRAME_TIMEOUT_US`] is lost: the cells it covers that no datagram that
/// arrived does are asked for again.
#[derive(Default)]
struct TileTracker {
    pending: Vec<PendingUpdate>,
}

struct PendingUpdate {
    epoch: u16,
    update: u32,
    got: Vec<bool>,
    left: usize,
    bounds: Rect,
    received: Vec<Rect>,
    first_us: u64,
}

/// Updates tracked at once; older ones are given up.
const MAX_PENDING: usize = 32;

impl TileTracker {
    fn receive(&mut self, h: &TilesHeader, body: &[u8], now_us: u64) {
        // A new epoch starts with the whole screen: forget the old one.
        self.pending.retain(|p| p.epoch == h.epoch);
        let i = match self.pending.iter().position(|p| p.update == h.update) {
            Some(i) => i,
            None => {
                if self.pending.len() == MAX_PENDING {
                    self.pending.remove(0);
                }
                self.pending.push(PendingUpdate {
                    epoch: h.epoch,
                    update: h.update,
                    got: vec![false; h.count as usize],
                    left: h.count as usize,
                    bounds: h.bounds,
                    received: Vec::new(),
                    first_us: now_us,
                });
                self.pending.len() - 1
            }
        };
        let p = &mut self.pending[i];
        let Some(got) = p.got.get_mut(h.index as usize) else { return };
        if !std::mem::replace(got, true) {
            p.left -= 1;
            p.received.extend(tiles::tile_rects(body));
        }
        if p.left == 0 {
            self.pending.remove(i);
        }
    }

    /// The regions to ask for again, from updates that have timed out.
    fn expire(&mut self, now_us: u64) -> Vec<Rect> {
        let mut out = Vec::new();
        self.pending.retain(|p| {
            if now_us.saturating_sub(p.first_us) < FRAME_TIMEOUT_US {
                return true;
            }
            out.extend(tiles::missing(p.bounds, &p.received));
            false
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(n: u32, refs: u32, keyframe: bool) -> FragmentHeader {
        FragmentHeader {
            flags: if keyframe { farsight_proto::video::FLAG_KEYFRAME } else { 0 },
            epoch: 1,
            frame: n,
            refs,
            index: 0,
            count: 1,
            data: 1,
            len: 0,
            capture_us: 0,
            encode_us: 0,
        }
    }

    #[test]
    fn the_chain_takes_what_it_can_decode_and_asks_for_the_rest() {
        let t = Instant::now();
        let retry = Duration::from_millis(50);
        let mut c = Chain::default();
        // Nothing until a keyframe.
        assert!(!c.admit(&header(4, 3, false)));
        assert_eq!(c.ask(t, retry), Some(Ask::Keyframe));
        assert!(c.admit(&header(5, 5, true)));
        assert!(c.admit(&header(6, 5, false)));
        assert_eq!(c.ask(t, retry), None);
        // 7 is lost: 8 and 9 can't be decoded; ask once per retry.
        c.lost(7);
        assert!(!c.admit(&header(8, 7, false)));
        assert_eq!(c.ask(t, retry), Some(Ask::Rfi { lost: 8, good: 6 }));
        assert!(!c.admit(&header(9, 8, false)));
        assert_eq!(c.ask(t + retry / 2, retry), None);
        assert_eq!(c.ask(t + retry, retry), Some(Ask::Rfi { lost: 9, good: 6 }));
        // The server predicts 10 from 6.
        assert!(c.admit(&header(10, 6, false)));
        assert!(c.admit(&header(11, 10, false)));
        assert_eq!(c.ask(t + 2 * retry, retry), None);
        // A late loss report for an older frame changes nothing.
        c.lost(9);
        assert_eq!(c.ask(t + 3 * retry, retry), None);
        // Nothing before the chain's keyframe will do.
        assert!(!c.admit(&header(12, 4, false)));
    }

    #[test]
    fn a_reset_wants_a_keyframe() {
        let mut c = Chain::default();
        assert!(c.admit(&header(1, 1, true)));
        c.reset();
        assert!(!c.admit(&header(2, 1, false)));
        assert_eq!(c.ask(Instant::now(), Duration::ZERO), Some(Ask::Keyframe));
    }

    fn frame(epoch: u16, n: u32) -> VideoFrame {
        let header = FragmentHeader { epoch, ..header(n, n.wrapping_sub(1), false) };
        VideoFrame { header, data: Vec::new(), first_us: 0, complete_us: 0 }
    }

    fn numbers(media: Vec<Media>) -> Vec<u32> {
        media
            .into_iter()
            .map(|m| match m {
                Media::Frame(f) => f.header.frame,
                Media::Tiles(t) => t.header.update,
            })
            .collect()
    }

    #[test]
    fn frames_wait_for_their_epoch() {
        let mut g = EpochGate::default();
        assert!(g.admit(Media::Frame(frame(1, 0))).is_none());
        assert!(g.admit(Media::Frame(frame(2, 1))).is_none());
        assert_eq!(numbers(g.announce(1)), [0]);
        assert!(g.admit(Media::Frame(frame(1, 2))).is_some());
        assert_eq!(numbers(g.announce(2)), [1]);
        // A late frame from the old epoch is dropped.
        assert!(g.admit(Media::Frame(frame(1, 3))).is_none());
        assert!(g.announce(3).is_empty());
        assert!(epoch_after(0, u16::MAX));
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;

    fn header(update: u32, index: u16, count: u16) -> TilesHeader {
        TilesHeader { epoch: 1, update, index, count, capture_us: 0, encode_us: 0, bounds: Rect::new(0, 0, 128, 64) }
    }

    fn body(x: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&[64, 64, 0, 0, 0]);
        b
    }

    #[test]
    fn lost_cells_are_asked_for_once() {
        let mut t = TileTracker::default();
        t.receive(&header(1, 0, 2), &body(0), 0);
        t.receive(&header(2, 0, 1), &body(0), 0); // complete
        assert!(t.expire(FRAME_TIMEOUT_US - 1).is_empty());
        assert_eq!(t.expire(FRAME_TIMEOUT_US), [Rect::new(64, 0, 64, 64)]);
        assert!(t.expire(2 * FRAME_TIMEOUT_US).is_empty());
    }
}
