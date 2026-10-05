//! Platform-independent client core: connection, codec negotiation, input
//! state sync and layout reporting. The desktop and Android apps wrap it.
//!
//! [`Client::connect`] runs on a tokio runtime and reports to the app
//! through a callback: video epochs, whole frames ready to decode, cursor
//! changes and the end of the connection. Frames are handed over in order
//! and only from a keyframe on: after a loss the core drops frames and asks
//! for a keyframe (RFI comes in M4), so the decoder never sees a damaged
//! stream (§2). Each epoch's `Epoch` event comes before its first frame,
//! even when the frame overtook the announcement on the wire.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::Reassembler;
use farsight_net::quinn::Connection;
use farsight_net::{endpoint, quinn, stream};
use farsight_proto::codec::{DecoderCaps, Format, Mode};
use farsight_proto::control::{ClientMessage, CursorImage, CursorShape, Epoch, Hello, ServerMessage, Welcome};
use farsight_proto::datagram::{Datagram, Ping};
use farsight_proto::input::{InputEvent, InputPacket, InputSender};
use farsight_proto::layout::Layout;
use farsight_proto::video::FragmentHeader;
use tokio::sync::mpsc;

/// A frame missing fragments this long after its first one is lost. Long
/// enough for a paced keyframe to arrive.
const FRAME_TIMEOUT_US: u64 = 250_000;

/// Keyframe requests are repeated this often until one arrives.
const KEYFRAME_RETRY: Duration = Duration::from_millis(100);

/// How often the clock offset to the server is measured.
const PING_INTERVAL: Duration = Duration::from_millis(500);

/// Clock samples kept; the one with the shortest round trip wins.
const PING_SAMPLES: usize = 16;

/// Frames held while their epoch's announcement is on its way.
const MAX_HELD: usize = 64;

/// The client core's version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub struct Config {
    pub addr: SocketAddr,
    pub layout: Layout,
    pub decoders: Vec<DecoderCaps>,
    pub mode: Mode,
}

#[derive(Debug)]
pub enum Event {
    Connected { fingerprint: String, welcome: Welcome },
    /// A new epoch: the frames after this are in its encoding and layout.
    Epoch(Epoch),
    /// A whole frame, ready to decode.
    Frame(VideoFrame),
    CursorImage(CursorImage),
    Cursor(CursorShape),
    /// The connection ended; the reason.
    Closed(String),
}

#[derive(Debug)]
pub struct VideoFrame {
    pub header: FragmentHeader,
    pub data: Vec<u8>,
    /// When its first and last fragments arrived, in [`Client::now_us`].
    pub first_us: u64,
    pub complete_us: u64,
}

/// Running counts, for the app's statistics.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub lost: u64,
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
    want_keyframe: AtomicBool,
    stats: Mutex<Stats>,
}

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
        let endpoint = endpoint::client()?;
        let (conn, fingerprint) = endpoint::connect(&endpoint, cfg.addr).await?;
        tracing::info!(addr = %cfg.addr, %fingerprint, "connected");
        let (mut send, mut recv) = conn.open_bi().await.context("opening the control stream")?;
        let hello = Hello { decoders: cfg.decoders, layout: cfg.layout, mode: cfg.mode };
        stream::send(&mut send, &ClientMessage::Hello(hello)).await?;
        let welcome = match stream::recv(&mut recv).await? {
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
            want_keyframe: AtomicBool::new(true),
            stats: Mutex::default(),
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
        self.shared.want_keyframe.store(true, Ordering::Relaxed);
        self.shared.stats.lock().unwrap().keyframe_requests += 1;
        let _ = self.control.send(ClientMessage::RequestKeyframe);
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
        self.shared.want_keyframe.store(true, Ordering::Relaxed);
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
                    for f in held {
                        on_event(Event::Frame(f));
                    }
                }
                ServerMessage::CursorImage(image) => on_event(Event::CursorImage(image)),
                ServerMessage::Cursor(shape) => on_event(Event::Cursor(shape)),
                ServerMessage::Welcome(_) => tracing::warn!("unexpected Welcome"),
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
        let mut samples: Vec<(u64, i64)> = Vec::new();
        let mut last_request: Option<Instant> = None;
        let request = |last: &mut Option<Instant>| {
            if last.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
                *last = Some(Instant::now());
                shared.stats.lock().unwrap().keyframe_requests += 1;
                let _ = control.send(ClientMessage::RequestKeyframe);
            }
        };
        let err = loop {
            let d = match tokio::time::timeout(Duration::from_millis(50), conn.read_datagram()).await {
                Ok(Ok(d)) => Some(d),
                Ok(Err(err)) => break err,
                Err(_) => None,
            };
            let now = shared.now_us();
            match d.as_deref().and_then(Datagram::decode) {
                Some(Datagram::Video(h, payload)) => {
                    if let Some(f) = rx.push(h, payload, now) {
                        let mut stats = shared.stats.lock().unwrap();
                        stats.frames += 1;
                        drop(stats);
                        if f.header.keyframe() {
                            shared.want_keyframe.store(false, Ordering::Relaxed);
                        }
                        if shared.want_keyframe.load(Ordering::Relaxed) {
                            request(&mut last_request);
                        } else {
                            let f = VideoFrame {
                                header: f.header,
                                data: f.data,
                                first_us: f.first_us,
                                complete_us: f.complete_us,
                            };
                            if let Some(f) = gate.lock().unwrap().admit(f) {
                                on_event(Event::Frame(f));
                            }
                        }
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
            rx.expire(now);
            let lost = rx.take_lost();
            if lost > 0 {
                shared.stats.lock().unwrap().lost += lost as u64;
                tracing::debug!(lost, "frames lost; waiting for a keyframe");
                shared.want_keyframe.store(true, Ordering::Relaxed);
                request(&mut last_request);
            }
        };
        Err::<(), _>(anyhow::Error::from(err))
    };
    let result = tokio::select! {
        r = writer => r,
        r = reader => r,
        r = datagrams => r,
        () = ticks => Ok(()),
    };
    let reason = match result {
        Ok(()) => "the server closed the connection".to_string(),
        Err(err) => format!("{err:#}"),
    };
    tracing::info!(%reason, "disconnected");
    conn.close(0u32.into(), b"");
    on_event(Event::Closed(reason));
}

/// Holds frames back until their epoch is announced, so the app always
/// knows a frame's encoding before it sees the frame.
#[derive(Default)]
struct EpochGate {
    known: Option<u16>,
    held: Vec<VideoFrame>,
}

/// `a` comes after `b`, across wrap-around.
fn epoch_after(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) > 0
}

impl EpochGate {
    /// The frame, if it can go to the app now. Frames of older epochs are
    /// dropped; frames of newer ones wait.
    fn admit(&mut self, f: VideoFrame) -> Option<VideoFrame> {
        match self.known {
            Some(e) if e == f.header.epoch => Some(f),
            Some(e) if epoch_after(e, f.header.epoch) => None,
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
    fn announce(&mut self, epoch: u16) -> Vec<VideoFrame> {
        self.known = Some(epoch);
        let held = std::mem::take(&mut self.held);
        let (now, later): (Vec<_>, Vec<_>) = held
            .into_iter()
            .filter(|f| !epoch_after(epoch, f.header.epoch))
            .partition(|f| f.header.epoch == epoch);
        self.held = later;
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(epoch: u16, n: u32) -> VideoFrame {
        let header = FragmentHeader { flags: 0, epoch, frame: n, index: 0, count: 1, capture_us: 0, encode_us: 0 };
        VideoFrame { header, data: Vec::new(), first_us: 0, complete_us: 0 }
    }

    #[test]
    fn frames_wait_for_their_epoch() {
        let mut g = EpochGate::default();
        assert!(g.admit(frame(1, 0)).is_none());
        assert!(g.admit(frame(2, 1)).is_none());
        let now: Vec<u32> = g.announce(1).iter().map(|f| f.header.frame).collect();
        assert_eq!(now, [0]);
        assert!(g.admit(frame(1, 2)).is_some());
        let now: Vec<u32> = g.announce(2).iter().map(|f| f.header.frame).collect();
        assert_eq!(now, [1]);
        // A late frame from the old epoch is dropped.
        assert!(g.admit(frame(1, 3)).is_none());
        assert!(g.announce(3).is_empty());
        assert!(epoch_after(0, u16::MAX));
    }
}
