//! A session with one server, as Kotlin sees it: [`Session`] connects, and
//! reconnects when the connection is lost (the session lives on at the
//! server, §6), and reports through a [`SessionListener`]. Video and audio
//! never come up to Kotlin: they go to [`Media`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use farsight_client::{Client, Config, Event, Refused};
use farsight_proto::codec::{Chroma, Codec, DecoderCaps, Format, Mode};
use farsight_proto::control::{ClipboardOffer, CursorImage, CursorShape};
use farsight_proto::input::InputEvent;
use farsight_proto::layout::Layout;
use tokio::sync::{Notify, oneshot};

use crate::FarsightError;
use crate::media::Media;

/// Text types on the clipboard, best first.
const TEXT: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

/// How often statistics go to the listener.
const STATS_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

/// A decoder on this device, as MediaCodecList describes it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VideoDecoder {
    pub codec: VideoCodec,
    /// MediaCodec's name for it, to create it by.
    pub name: String,
    pub max_width: u32,
    pub max_height: u32,
    pub hardware: bool,
}

impl VideoDecoder {
    pub(crate) fn format(&self) -> Format {
        let codec = match self.codec {
            VideoCodec::H264 => Codec::H264,
            VideoCodec::Hevc => Codec::Hevc,
            VideoCodec::Av1 => Codec::Av1,
        };
        Format { codec, chroma: Chroma::Yuv420, bit_depth: 8 }
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn mime(&self) -> &'static str {
        match self.codec {
            VideoCodec::H264 => "video/avc",
            VideoCodec::Hevc => "video/hevc",
            VideoCodec::Av1 => "video/av01",
        }
    }
}

/// The remote output, as the client wants it or as the server applied it
/// (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ScreenLayout {
    pub width_px: u32,
    pub height_px: u32,
    /// In 1/120 steps: 120 is 1×.
    pub scale_120: u32,
    pub refresh_mhz: u32,
}

impl From<ScreenLayout> for Layout {
    fn from(l: ScreenLayout) -> Self {
        Layout { width_px: l.width_px, height_px: l.height_px, scale_120: l.scale_120, refresh_mhz: l.refresh_mhz }
    }
}

impl From<Layout> for ScreenLayout {
    fn from(l: Layout) -> Self {
        ScreenLayout { width_px: l.width_px, height_px: l.height_px, scale_120: l.scale_120, refresh_mhz: l.refresh_mhz }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionOptions {
    /// Holds this device's key (`client_key`) and the servers it knows
    /// (`known_hosts`).
    pub config_dir: String,
    /// `host[:port]`.
    pub address: String,
    /// Plaintext mode (§1), for a server on a tailnet.
    pub plain: bool,
    pub layout: ScreenLayout,
    pub decoders: Vec<VideoDecoder>,
    /// Prefer motion to sharp text (§3).
    pub motion: bool,
    pub audio: bool,
    /// The microphone may be asked for (the user allows it each time, or
    /// always).
    pub mic: bool,
    pub view_only: bool,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum RemoteCursor {
    Hidden,
    /// A CSS cursor name, from the device's own idea of it.
    Named { name: String },
    /// An image sent earlier.
    Image { id: u64 },
}

/// A cursor image, ready for `Bitmap.copyPixelsFromBuffer`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CursorBitmap {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// Image pixels per output pixel, in 1/120 steps.
    pub scale_120: u32,
    /// Premultiplied RGBA, rows packed.
    pub rgba: Vec<u8>,
}

impl From<CursorImage> for CursorBitmap {
    fn from(image: CursorImage) -> Self {
        let mut rgba = image.pixels;
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2); // BGRA → RGBA
        }
        CursorBitmap {
            id: image.id,
            width: image.width,
            height: image.height,
            hotspot_x: image.hotspot.0,
            hotspot_y: image.hotspot.1,
            scale_120: image.scale_120,
            rgba,
        }
    }
}

/// What the overlay shows, once a second.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct SessionStats {
    pub encoding: String,
    pub fps: f32,
    /// Medians over the last second, in ms: the server's capture to the
    /// frame's last datagram, to the decoder's output, and to the display.
    pub network_ms: f32,
    pub decode_ms: f32,
    pub total_ms: f32,
    pub rtt_ms: f32,
    /// Frames lost after FEC and NACK, since connecting.
    pub lost: u64,
    /// The session's audio, capture to speaker, in ms.
    pub audio_ms: f32,
    pub mic_on: bool,
}

/// How the session talks back to Kotlin. Called from the session's own
/// threads.
#[uniffi::export(with_foreign)]
pub trait SessionListener: Send + Sync {
    fn connected(&self, fingerprint: String);
    /// The connection was lost; trying again.
    fn reconnecting(&self, attempt: u32, reason: String);
    /// A new video epoch, in this layout (the server's, which may differ
    /// from the one asked for) and encoding.
    fn epoch(&self, layout: ScreenLayout, encoding: String);
    fn cursor_image(&self, image: CursorBitmap);
    fn cursor(&self, cursor: RemoteCursor);
    /// Text on the session's clipboard.
    fn clipboard(&self, text: String);
    /// A text field in the session gained or lost focus.
    fn text_input(&self, active: bool);
    /// An app in the session started or stopped recording: call
    /// [`Session::set_mic`] if the user allows it.
    fn mic_demand(&self, on: bool);
    fn stats(&self, stats: SessionStats);
    /// The session is over. `refused`: connecting again won't help until
    /// something changes (a key, a pin).
    fn closed(&self, reason: String, refused: bool);
}

#[derive(uniffi::Object)]
pub struct Session {
    inner: Arc<Inner>,
    /// Here rather than in `Inner`, which its own tasks hold: a runtime
    /// can't be dropped on one of its threads.
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
}

struct Inner {
    runtime: tokio::runtime::Handle,
    listener: Arc<dyn SessionListener>,
    options: Mutex<SessionOptions>,
    key: Arc<farsight_net::auth::ClientKey>,
    client: Mutex<Option<Arc<Client>>>,
    /// A clipboard offer that came before the connection was handed over.
    offer: Mutex<Option<ClipboardOffer>>,
    media: Media,
    closing: AtomicBool,
    wake: Notify,
}

/// The wait before reconnecting: 0.5 s, doubling to 5 s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << attempt.min(4)).min(5000))
}

#[uniffi::export]
impl Session {
    /// Starts connecting; the listener hears how it goes.
    #[uniffi::constructor]
    pub fn new(options: SessionOptions, listener: Arc<dyn SessionListener>) -> Result<Arc<Self>, FarsightError> {
        let key = Arc::new(crate::key(&options.config_dir)?);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("farsight-net")
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)?;
        let media = Media::new(options.decoders.clone());
        let inner = Arc::new(Inner {
            runtime: runtime.handle().clone(),
            listener,
            options: Mutex::new(options),
            key,
            client: Mutex::default(),
            offer: Mutex::default(),
            media,
            closing: AtomicBool::new(false),
            wake: Notify::new(),
        });
        runtime.spawn(run(inner.clone()));
        runtime.spawn(report(inner.clone()));
        Ok(Arc::new(Session { inner, runtime: Mutex::new(Some(runtime)) }))
    }

    /// Absolute pointer position, in the remote output's pixels.
    pub fn pointer(&self, x: f32, y: f32) {
        self.inner.input(InputEvent::PointerAbs { x, y });
    }

    /// Relative pointer motion, in the remote output's pixels.
    pub fn pointer_relative(&self, dx: f32, dy: f32) {
        self.inner.input(InputEvent::PointerRel { dx, dy });
    }

    /// An evdev button code (`BTN_LEFT` is 0x110).
    pub fn button(&self, code: u32, pressed: bool) {
        self.inner.input(InputEvent::Button { code, pressed });
    }

    /// An evdev key code.
    pub fn key(&self, code: u32, pressed: bool) {
        self.inner.input(InputEvent::Key { code, pressed });
    }

    /// Scroll in remote pixels (down and right are positive), plus wheel
    /// clicks in 1/120 steps (0 for smooth scrolling).
    pub fn scroll(&self, dx: f32, dy: f32, v120_x: i32, v120_y: i32) {
        self.inner.input(InputEvent::Scroll { dx, dy, v120_x, v120_y });
    }

    /// Nothing is held any more, as when the app loses focus.
    pub fn release_all(&self) {
        if let Some(c) = self.inner.client() {
            c.release_all();
        }
    }

    /// The screen's size or scale changed (§5).
    pub fn set_layout(&self, layout: ScreenLayout) {
        let mut options = self.inner.options.lock().unwrap();
        if options.layout == layout || options.view_only {
            return;
        }
        options.layout = layout;
        if let Some(c) = self.inner.client() {
            tracing::info!(?layout, "SetLayout");
            c.set_layout(layout.into());
        }
    }

    pub fn set_motion(&self, motion: bool) {
        self.inner.options.lock().unwrap().motion = motion;
        if let Some(c) = self.inner.client() {
            c.set_mode(if motion { Mode::Motion } else { Mode::Text });
        }
    }

    /// Text typed on the soft keyboard, for the focused field.
    pub fn commit_text(&self, text: String) {
        if let Some(c) = self.inner.client().filter(|_| !self.inner.view_only()) {
            c.commit_text(text);
        }
    }

    /// Text being composed; empty clears it. The cursor is a byte range in
    /// it, or none if `begin` is negative.
    pub fn preedit(&self, text: String, begin: i32, end: i32) {
        if let Some(c) = self.inner.client().filter(|_| !self.inner.view_only()) {
            let cursor = (begin >= 0 && end >= begin).then_some((begin as u32, end as u32));
            c.preedit(text, cursor);
        }
    }

    /// The device's clipboard holds this text: offer it to the session.
    pub fn offer_clipboard(&self, text: String) {
        let Some(c) = self.inner.client().filter(|_| !self.inner.view_only()) else { return };
        let text = Arc::new(text);
        c.offer_clipboard(
            TEXT.iter().map(|t| t.to_string()).collect(),
            Arc::new(move |mime| TEXT.contains(&mime).then(|| text.as_bytes().to_vec())),
        );
    }

    /// Plays the session's audio, or mutes it at the server.
    pub fn set_audio(&self, play: bool) {
        self.inner.options.lock().unwrap().audio = play;
        if let Some(c) = self.inner.client() {
            c.set_audio(play);
        }
    }

    /// Opens or closes the microphone, while the session wants it. The
    /// app holds RECORD_AUDIO, and with `echo_cancel` has put the device in
    /// communication mode; without it (headphones), nothing is cancelled.
    pub fn set_mic(&self, on: bool, echo_cancel: bool) -> Result<(), FarsightError> {
        Ok(self.inner.media.set_mic(on, echo_cancel)?)
    }

    /// Ends the session here; it lives on at the server.
    pub fn disconnect(&self) {
        self.inner.closing.store(true, Ordering::Relaxed);
        self.inner.wake.notify_waiters();
        if let Some(c) = self.inner.client.lock().unwrap().take() {
            c.close();
        }
        self.inner.media.set_client(None);
        self.inner.media.stop_audio();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
        if let Some(runtime) = self.runtime.lock().unwrap().take() {
            runtime.shutdown_background();
        }
    }
}

impl Inner {
    fn client(&self) -> Option<Arc<Client>> {
        self.client.lock().unwrap().clone()
    }

    fn view_only(&self) -> bool {
        self.options.lock().unwrap().view_only
    }

    fn input(&self, event: InputEvent) {
        if let Some(c) = self.client().filter(|_| !self.view_only()) {
            c.input(event);
        }
    }

    fn config(&self) -> anyhow::Result<Config> {
        let o = self.options.lock().unwrap().clone();
        let decoders = o
            .decoders
            .iter()
            .map(|d| DecoderCaps {
                format: d.format(),
                max_width: d.max_width,
                max_height: d.max_height,
                hardware: d.hardware,
                partial_decode: false,
            })
            .collect();
        Ok(Config {
            addr: farsight_client::resolve(o.address.trim())?,
            server_name: farsight_client::server_name(o.address.trim()),
            key: self.key.clone(),
            known_hosts: farsight_net::auth::KnownHosts::new(Path::new(&o.config_dir).join("known_hosts")),
            plain: o.plain,
            layout: o.layout.into(),
            decoders,
            mode: if o.motion { Mode::Motion } else { Mode::Text },
            audio: o.audio.then_some(farsight_proto::audio::AudioCaps { max_channels: 2 }),
            view_only: o.view_only,
            mic: o.mic && !o.view_only,
        })
    }

    fn event(self: &Arc<Self>, event: Event, closed: &Mutex<Option<oneshot::Sender<(String, bool)>>>) {
        match event {
            Event::Connected { fingerprint, .. } => self.listener.connected(fingerprint),
            Event::Epoch(e) => {
                self.media.epoch(&e);
                self.listener.epoch(e.layout.into(), e.encoding.to_string());
            }
            Event::Frame(f) => self.media.frame(f),
            Event::Tiles(t) => self.media.tiles(t),
            Event::CursorImage(image) => self.listener.cursor_image(image.into()),
            Event::Cursor(shape) => self.listener.cursor(match shape {
                CursorShape::Hidden => RemoteCursor::Hidden,
                CursorShape::Named(name) => RemoteCursor::Named { name },
                CursorShape::Image(id) => RemoteCursor::Image { id },
            }),
            Event::AudioConfig(config) => self.media.audio_config(config),
            Event::ClipboardOffer(offer) => match self.client() {
                Some(client) => self.fetch_clipboard(client, offer),
                None => *self.offer.lock().unwrap() = Some(offer),
            },
            Event::TextInput(active) => self.listener.text_input(active),
            Event::MicDemand(on) => {
                if !on {
                    let _ = self.media.set_mic(false, false);
                }
                self.listener.mic_demand(on);
            }
            Event::Closed { reason, retry } => {
                if let Some(tx) = closed.lock().unwrap().take() {
                    let _ = tx.send((reason, retry));
                }
            }
        }
    }

    fn fetch_clipboard(self: &Arc<Self>, client: Arc<Client>, offer: ClipboardOffer) {
        let Some(mime) = TEXT.iter().find(|t| offer.mimes.iter().any(|m| m == *t)) else { return };
        let inner = self.clone();
        self.runtime.spawn(async move {
            match client.fetch_clipboard(offer.serial, mime).await {
                Ok(data) => inner.listener.clipboard(String::from_utf8_lossy(&data).into_owned()),
                Err(err) => tracing::debug!("fetching the session's clipboard: {err:#}"),
            }
        });
    }

    /// Waits `d`, or less if the session is closing.
    async fn sleep(&self, d: Duration) {
        let _ = tokio::time::timeout(d, self.wake.notified()).await;
    }
}

async fn run(inner: Arc<Inner>) {
    let (mut attempt, mut connected_once) = (0u32, false);
    loop {
        if inner.closing.load(Ordering::Relaxed) {
            return;
        }
        let cfg = match inner.config() {
            Ok(cfg) => cfg,
            Err(err) if connected_once => {
                attempt += 1;
                inner.listener.reconnecting(attempt, format!("{err:#}"));
                inner.sleep(backoff(attempt)).await;
                continue;
            }
            Err(err) => return inner.listener.closed(format!("{err:#}"), false),
        };
        tracing::info!(addr = %cfg.addr, layout = ?cfg.layout, "connecting");
        let (closed_tx, closed_rx) = oneshot::channel();
        let closed_tx = Mutex::new(Some(closed_tx));
        let on_event = {
            let inner = inner.clone();
            move |e| inner.event(e, &closed_tx)
        };
        match Client::connect(cfg, on_event).await {
            Ok(client) => {
                let client = Arc::new(client);
                if inner.closing.load(Ordering::Relaxed) {
                    client.close();
                    return;
                }
                *inner.client.lock().unwrap() = Some(client.clone());
                inner.media.set_client(Some(client.clone()));
                if let Some(offer) = inner.offer.lock().unwrap().take() {
                    inner.fetch_clipboard(client.clone(), offer);
                }
                // The screen may have changed while connecting.
                let layout = inner.options.lock().unwrap().layout;
                client.set_layout(layout.into());
                (connected_once, attempt) = (true, 0);
                let (reason, retry) = closed_rx.await.unwrap_or_else(|_| ("closed".into(), false));
                *inner.client.lock().unwrap() = None;
                inner.media.set_client(None);
                inner.media.stop_audio();
                if inner.closing.load(Ordering::Relaxed) {
                    return;
                }
                if !retry {
                    return inner.listener.closed(reason, false);
                }
                attempt += 1;
                inner.listener.reconnecting(attempt, reason);
            }
            Err(err) => {
                let refused = err.is::<Refused>();
                if inner.closing.load(Ordering::Relaxed) {
                    return;
                }
                if !connected_once || refused {
                    return inner.listener.closed(format!("{err:#}"), refused);
                }
                attempt += 1;
                inner.listener.reconnecting(attempt, format!("{err:#}"));
            }
        }
        inner.sleep(backoff(attempt)).await;
    }
}

/// Statistics to the listener every second, and to the log every five.
async fn report(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(STATS_EVERY);
    let mut n = 0u32;
    while !inner.closing.load(Ordering::Relaxed) {
        tick.tick().await;
        let Some(client) = inner.client() else { continue };
        let core = client.stats();
        let media = inner.media.stats();
        let audio = client.audio_stats();
        let audio_ms = audio
            .as_ref()
            .and_then(|s| {
                let mut l = s.latency_us.clone();
                l.sort_unstable();
                l.get(l.len() / 2).copied()
            })
            .map_or(0.0, |us| us as f32 / 1000.0);
        let stats = SessionStats {
            encoding: media.encoding,
            fps: media.fps,
            network_ms: media.network_ms,
            decode_ms: media.decode_ms,
            total_ms: media.total_ms,
            rtt_ms: core.rtt_us as f32 / 1000.0,
            lost: core.lost,
            audio_ms,
            mic_on: media.mic_on,
        };
        n += 1;
        if n.is_multiple_of(5) {
            tracing::info!(
                "latency ms: network {:.1} decode {:.1} total {:.1}; fps {:.0}; rtt {:.1}; lost {}; audio {:.1}; {}",
                stats.network_ms,
                stats.decode_ms,
                stats.total_ms,
                stats.fps,
                stats.rtt_ms,
                stats.lost,
                stats.audio_ms,
                stats.encoding
            );
            if let Some(a) = audio.filter(|a| !a.latency_us.is_empty() || a.underruns > 0) {
                tracing::info!(
                    "audio: buffer {:.1} ms (target {:.1}); output {:.1} ms; concealed {} (late {}) underruns {} skipped {}",
                    a.buffered_us as f64 / 1000.0,
                    a.target_us as f64 / 1000.0,
                    a.output_delay_us as f64 / 1000.0,
                    a.concealed,
                    a.late,
                    a.underruns,
                    a.skipped
                );
            }
        }
        inner.listener.stats(stats);
    }
}
