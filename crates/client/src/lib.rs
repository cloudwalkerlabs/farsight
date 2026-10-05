//! Platform-independent client core: connection, codec negotiation, input
//! state sync and layout reporting. The desktop and Android apps wrap it.
//!
//! [`Client::connect`] runs on a tokio runtime and reports to the app
//! through a callback: whole frames ready to decode, cursor changes and the
//! end of the connection. Frames are handed over in order and only from a
//! keyframe on: after a loss the core drops frames and asks for a keyframe
//! (RFI comes in M4), so the decoder never sees a damaged stream (§2).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use farsight_net::packetize::Reassembler;
use farsight_net::quinn::Connection;
use farsight_net::{endpoint, quinn, stream};
use farsight_proto::codec::DecoderCaps;
use farsight_proto::control::{ClientMessage, CursorImage, CursorShape, Hello, ServerMessage, Welcome};
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

/// The client core's version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub struct Config {
    pub addr: SocketAddr,
    pub layout: Layout,
    pub decoders: Vec<DecoderCaps>,
}

#[derive(Debug)]
pub enum Event {
    Connected { fingerprint: String, welcome: Welcome },
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
        stream::send(&mut send, &ClientMessage::Hello(Hello { decoders: cfg.decoders, layout: cfg.layout })).await?;
        let welcome = match stream::recv(&mut recv).await? {
            Some(ServerMessage::Welcome(w)) => w,
            Some(other) => anyhow::bail!("expected Welcome, got {other:?}"),
            None => anyhow::bail!("the server closed the control stream"),
        };
        tracing::info!(codec = ?welcome.codec, layout = ?welcome.layout, "welcome");

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
    let reader = async {
        while let Some(msg) = stream::recv::<ServerMessage>(&mut recv).await? {
            match msg {
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
                            on_event(Event::Frame(VideoFrame {
                                header: f.header,
                                data: f.data,
                                first_us: f.first_us,
                                complete_us: f.complete_us,
                            }));
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
