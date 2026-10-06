//! The video thread. Frames from the connection go to MediaCodec, which
//! draws them into the session view's surface; tiles (when the server has
//! no hardware encoder) are decoded here and drawn into the same surface
//! by the CPU, into a copy of the screen kept here.
//!
//! The codec lives as long as the epoch's format and size, and the
//! surface: one that goes away (the app went to the background) takes the
//! codec with it, and the next one starts from a keyframe. Each frame's
//! timing is kept until the display shows it, for glass-to-glass latency.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

use farsight_client::{Client, TilesPacket, VideoFrame};
use farsight_proto::codec::{Codec as CodecId, Encoding, Format};
use farsight_proto::tiles::Rect;
use ndk::hardware_buffer_format::HardwareBufferFormat;
use ndk::native_window::NativeWindow;

use super::codec::{Codec, CodecEvent, FLAG_CODEC_CONFIG, FLAG_KEY_FRAME};
use super::super::annexb::split_parameter_sets;
use super::surface;
use crate::session::VideoDecoder;

/// Decoder failures in a row before its format is given up (§3).
const MAX_FAILURES: u32 = 3;

/// Frames waiting for input buffers before the codec is taken as stuck.
const MAX_QUEUED: usize = 30;

/// Frames whose timing is kept until they are shown.
const MAX_TIMED: usize = 64;

pub enum Msg {
    Epoch { encoding: Encoding, size: (u32, u32) },
    Frame(VideoFrame),
    Tiles(TilesPacket),
    Client(Option<Arc<Client>>),
    /// The surface changed; answered once the old one is let go of.
    Surface(SyncSender<()>),
    Codec { generation: u64, event: CodecEvent },
    Stop,
}

/// One frame's way, each on the client's clock in µs.
#[derive(Debug, Clone, Copy)]
struct Timing {
    pts_us: u64,
    /// The server's capture, on its clock, as the frame's header gives it.
    server_capture_us: u64,
    /// The server's capture, converted.
    capture_us: Option<u64>,
    complete_us: u64,
    decoded_us: Option<u64>,
}

/// Samples since the statistics were last taken.
#[derive(Default)]
pub struct Samples {
    pub encoding: String,
    pub shown: u32,
    pub network_us: Vec<u64>,
    pub decode_us: Vec<u64>,
    pub total_us: Vec<u64>,
}

pub struct Video {
    id: u64,
    tx: Sender<Msg>,
    samples: Arc<Mutex<Samples>>,
}

impl Video {
    pub fn spawn(decoders: Vec<VideoDecoder>) -> Self {
        let (tx, rx) = mpsc::channel();
        let samples = Arc::new(Mutex::new(Samples::default()));
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        surface::watch(id, tx.clone());
        let state = State {
            decoders,
            tx: tx.clone(),
            client: None,
            surface: surface::current(),
            encoding: None,
            size: (0, 0),
            video: None,
            generation: 0,
            need_keyframe: true,
            asked_keyframe: false,
            failures: 0,
            tiles: None,
            samples: samples.clone(),
        };
        std::thread::Builder::new()
            .name("farsight-video".into())
            .spawn(move || run(rx, state))
            .expect("spawning the video thread");
        Video { id, tx, samples }
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    pub fn take_samples(&self) -> Samples {
        let mut s = self.samples.lock().unwrap();
        let encoding = s.encoding.clone();
        Samples { encoding, ..std::mem::take(&mut *s) }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        surface::unwatch(self.id);
        let _ = self.tx.send(Msg::Stop);
    }
}

fn run(rx: Receiver<Msg>, mut state: State) {
    while let Ok(msg) = rx.recv() {
        if !state.handle(msg) {
            return;
        }
        // Draw tiles once what has arrived is decoded, not per datagram.
        while let Ok(msg) = rx.try_recv() {
            if !state.handle(msg) {
                return;
            }
        }
        state.present_tiles();
    }
}

struct Active {
    codec: Codec,
    generation: u64,
    format: Format,
    size: (u32, u32),
    /// Free input buffers, and what waits for one.
    inputs: VecDeque<usize>,
    queued: VecDeque<(Vec<u8>, u64, u32)>,
    timings: VecDeque<Timing>,
}

/// The screen as tiles draw it.
struct TileScreen {
    decoder: farsight_tiles::Decoder,
    size: (u32, u32),
    /// RGBX, rows `4 * width` bytes.
    pixels: Vec<u8>,
    /// What changed since it was last drawn: left, top, right, bottom.
    dirty: Option<(u32, u32, u32, u32)>,
    /// The surface's buffers are set up for it.
    ready: bool,
}

struct State {
    decoders: Vec<VideoDecoder>,
    tx: Sender<Msg>,
    client: Option<Arc<Client>>,
    surface: Option<NativeWindow>,
    encoding: Option<Encoding>,
    size: (u32, u32),
    video: Option<Active>,
    generation: u64,
    need_keyframe: bool,
    asked_keyframe: bool,
    failures: u32,
    tiles: Option<TileScreen>,
    samples: Arc<Mutex<Samples>>,
}

impl State {
    /// False once told to stop.
    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Stop => return false,
            Msg::Client(client) => self.client = client,
            Msg::Epoch { encoding, size } => self.epoch(encoding, size),
            Msg::Frame(f) => self.frame(f),
            Msg::Tiles(t) => self.tiles(t),
            Msg::Surface(ack) => {
                self.surface_changed();
                let _ = ack.try_send(());
            }
            Msg::Codec { generation, event } => self.codec_event(generation, event),
        }
        true
    }

    fn epoch(&mut self, encoding: Encoding, size: (u32, u32)) {
        tracing::info!(%encoding, ?size, "new epoch");
        self.encoding = Some(encoding);
        self.size = size;
        self.samples.lock().unwrap().encoding = encoding.to_string();
        match encoding {
            Encoding::Video(format) => {
                self.tiles = None;
                if self.video.as_ref().is_some_and(|v| (v.format, v.size) != (format, size)) {
                    self.video = None;
                }
            }
            Encoding::Tiles => {
                self.video = None;
                let screen = TileScreen {
                    decoder: match farsight_tiles::Decoder::new() {
                        Ok(d) => d,
                        Err(err) => return tracing::error!("tiles: {err:#}"),
                    },
                    size,
                    pixels: vec![0; size.0 as usize * size.1 as usize * 4],
                    dirty: None,
                    ready: false,
                };
                self.tiles = Some(screen);
            }
        }
    }

    fn surface_changed(&mut self) {
        let window = surface::current();
        if window.as_ref().map(NativeWindow::ptr) == self.surface.as_ref().map(NativeWindow::ptr) {
            return;
        }
        self.surface = window;
        match &self.surface {
            None => {
                // Nothing to draw on: the codec goes, and a keyframe will
                // be needed.
                self.video = None;
                if let Some(t) = &mut self.tiles {
                    t.ready = false;
                }
            }
            Some(window) => {
                if let Some(v) = &self.video
                    && let Err(err) = v.codec.set_surface(window)
                {
                    tracing::info!("moving the decoder to the new surface: {err:#}");
                    self.video = None;
                }
                if let Some(t) = &mut self.tiles {
                    t.ready = false;
                }
            }
        }
    }

    /// The decoder for the current epoch, if there can be one now.
    fn ensure_codec(&mut self, format: Format) -> bool {
        if self.video.as_ref().is_some_and(|v| (v.format, v.size) == (format, self.size)) {
            return true;
        }
        self.video = None;
        let Some(window) = &self.surface else { return false };
        let Some(d) = self.decoders.iter().find(|d| d.format() == format) else {
            tracing::warn!(%format, "no decoder for it");
            self.give_up(format);
            return false;
        };
        self.generation += 1;
        let (tx, generation) = (self.tx.clone(), self.generation);
        let sink = move |event| {
            let _ = tx.send(Msg::Codec { generation, event });
        };
        match Codec::new(&d.name, d.mime(), self.size, window, sink) {
            Ok(codec) => {
                tracing::info!(decoder = d.name, %format, size = ?self.size, "decoding");
                self.video = Some(Active {
                    codec,
                    generation,
                    format,
                    size: self.size,
                    inputs: VecDeque::new(),
                    queued: VecDeque::new(),
                    timings: VecDeque::new(),
                });
                self.need_keyframe = true;
                self.asked_keyframe = false;
                true
            }
            Err(err) => {
                tracing::warn!("{err:#}");
                self.failed(format);
                false
            }
        }
    }

    /// The decoder failed; after a few times, its format is given up.
    fn failed(&mut self, format: Format) {
        self.video = None;
        self.need_keyframe = true;
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.give_up(format);
        }
    }

    fn give_up(&mut self, format: Format) {
        tracing::warn!(%format, "giving up on the decoder");
        self.failures = 0;
        self.encoding = None;
        if let Some(c) = &self.client {
            c.decoder_failed(format);
        }
    }

    fn frame(&mut self, f: VideoFrame) {
        let Some(Encoding::Video(format)) = self.encoding else { return };
        if !self.ensure_codec(format) {
            return;
        }
        let keyframe = f.header.keyframe();
        if self.need_keyframe && !keyframe {
            if !std::mem::replace(&mut self.asked_keyframe, true)
                && let Some(c) = &self.client
            {
                c.request_keyframe();
            }
            return;
        }
        self.need_keyframe = false;
        let capture_us = self.client.as_ref().and_then(|c| c.server_to_local(f.header.capture_us));
        let v = self.video.as_mut().unwrap();
        let pts_us = f.complete_us;
        if v.timings.len() == MAX_TIMED {
            v.timings.pop_front();
        }
        v.timings.push_back(Timing {
            pts_us,
            server_capture_us: f.header.capture_us,
            capture_us,
            complete_us: f.complete_us,
            decoded_us: None,
        });
        if keyframe && format.codec != CodecId::Av1 {
            let (config, picture) = split_parameter_sets(format.codec, &f.data);
            if !config.is_empty() {
                v.queued.push_back((config, pts_us, FLAG_CODEC_CONFIG));
            }
            v.queued.push_back((picture, pts_us, FLAG_KEY_FRAME));
        } else {
            v.queued.push_back((f.data, pts_us, if keyframe { FLAG_KEY_FRAME } else { 0 }));
        }
        if v.queued.len() > MAX_QUEUED {
            tracing::warn!(queued = v.queued.len(), "the decoder takes no input");
            self.failed(format);
            return;
        }
        self.feed();
    }

    fn feed(&mut self) {
        let Some(v) = &mut self.video else { return };
        while !v.inputs.is_empty() && !v.queued.is_empty() {
            let index = v.inputs.pop_front().unwrap();
            let (data, pts_us, flags) = v.queued.pop_front().unwrap();
            if let Err(err) = v.codec.queue(index, &data, pts_us, flags) {
                tracing::warn!("queueing a frame: {err:#}");
            }
        }
    }

    fn codec_event(&mut self, generation: u64, event: CodecEvent) {
        let now_us = self.client.as_ref().map(|c| c.now_us());
        let Some(v) = self.video.as_mut().filter(|v| v.generation == generation) else { return };
        match event {
            CodecEvent::Input(index) => {
                v.inputs.push_back(index);
                self.feed();
            }
            CodecEvent::Output { index, pts_us } => {
                if let Err(err) = v.codec.render(index, monotonic_ns()) {
                    tracing::warn!("rendering a frame: {err:#}");
                }
                self.failures = 0;
                if let Some(t) = v.timings.iter_mut().find(|t| t.pts_us as i64 == pts_us) {
                    t.decoded_us = now_us;
                    if let Some(c) = &self.client {
                        c.decoded(t.server_capture_us);
                    }
                }
            }
            CodecEvent::Rendered { pts_us, system_ns } => {
                let Some(at) = v.timings.iter().position(|t| t.pts_us as i64 == pts_us) else { return };
                let t = v.timings.remove(at).unwrap();
                let (Some(now_us), Some(capture_us), Some(decoded_us)) = (now_us, t.capture_us, t.decoded_us) else {
                    return;
                };
                let shown_us = now_us.saturating_sub((monotonic_ns().saturating_sub(system_ns) / 1000) as u64);
                let mut s = self.samples.lock().unwrap();
                s.shown += 1;
                s.network_us.push(t.complete_us.saturating_sub(capture_us));
                s.decode_us.push(decoded_us.saturating_sub(t.complete_us));
                s.total_us.push(shown_us.saturating_sub(capture_us));
            }
            CodecEvent::Error { fatal, detail } => {
                tracing::warn!(fatal, "decoder: {detail}");
                if fatal {
                    let format = v.format;
                    self.failed(format);
                }
            }
        }
    }

    fn tiles(&mut self, t: TilesPacket) {
        let Some(screen) = &mut self.tiles else { return };
        let (w, h) = screen.size;
        let (pixels, dirty) = (&mut screen.pixels, &mut screen.dirty);
        let result = screen.decoder.decode(&t.body, |rect: Rect, px: &[u8]| {
            let (x, y, rw, rh) = (rect.x as u32, rect.y as u32, rect.w as u32, rect.h as u32);
            if x + rw > w || y + rh > h {
                return;
            }
            for row in 0..rh {
                let src = &px[(row * rw * 4) as usize..((row + 1) * rw * 4) as usize];
                let at = (((y + row) * w + x) * 4) as usize;
                pixels[at..at + src.len()].copy_from_slice(src);
            }
            let r = (x, y, x + rw, y + rh);
            *dirty = Some(dirty.map_or(r, |d| (d.0.min(r.0), d.1.min(r.1), d.2.max(r.2), d.3.max(r.3))));
        });
        if let Err(err) = result {
            tracing::warn!(update = t.header.update, "{err:#}");
        }
        // An update's last datagram counts, as for the desktop.
        if t.header.index + 1 == t.header.count
            && let Some(c) = &self.client
        {
            c.decoded(t.header.capture_us);
            if let Some(capture_us) = c.server_to_local(t.header.capture_us) {
                let mut s = self.samples.lock().unwrap();
                s.shown += 1;
                s.network_us.push(t.received_us.saturating_sub(capture_us));
                s.decode_us.push(c.now_us().saturating_sub(t.received_us));
                s.total_us.push(c.now_us().saturating_sub(capture_us));
            }
        }
    }

    /// Copies what tiles changed into the surface.
    fn present_tiles(&mut self) {
        let (Some(screen), Some(window)) = (&mut self.tiles, &self.surface) else { return };
        let (w, h) = screen.size;
        if !screen.ready {
            if let Err(err) = window.set_buffers_geometry(w as i32, h as i32, Some(HardwareBufferFormat::R8G8B8X8_UNORM)) {
                return tracing::warn!("tiles: setting up the surface: {err}");
            }
            screen.ready = true;
            screen.dirty = Some((0, 0, w, h));
        }
        let Some((left, top, right, bottom)) = screen.dirty.take() else { return };
        let mut rect = ndk::native_window::Rect { left: left as i32, top: top as i32, right: right as i32, bottom: bottom as i32 };
        let mut buffer = match window.lock(Some(&mut rect)) {
            Ok(b) => b,
            Err(err) => return tracing::warn!("tiles: drawing: {err}"),
        };
        let stride = buffer.stride();
        let bits = buffer.bits() as *mut u8;
        let clamp = |v: i32, max: u32| (v.max(0) as u32).min(max);
        let (l, t, r, b) = (clamp(rect.left, w), clamp(rect.top, h), clamp(rect.right, w), clamp(rect.bottom, h));
        let (bw, bh) = (buffer.width() as u32, buffer.height() as u32);
        if (bw, bh) != (w, h) {
            return tracing::debug!(buffer = ?(bw, bh), screen = ?(w, h), "tiles: the surface isn't the screen's size yet");
        }
        for y in t..b {
            let src = &screen.pixels[((y * w + l) * 4) as usize..((y * w + r) * 4) as usize];
            // SAFETY: the locked buffer has `stride` pixels per row and at
            // least `h` rows, and (l..r, y) is inside it.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), bits.add((y as usize * stride + l as usize) * 4), src.len());
            }
        }
    }
}

fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}
