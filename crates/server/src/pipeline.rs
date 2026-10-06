//! Per-frame work (docs/design.md §2, steps 1–5): nested buffer → RGB→YUV
//! shader passes on the main thread → encoder on the encode thread → the
//! network, and optionally a file.
//!
//! The main thread only submits the conversion (or, for encoders that take
//! memory, reads it back). The encode thread waits for its fence, encodes
//! and hands the packet on, so the event loop is free for the nested
//! compositor's next frame. Frame callbacks go out as soon as the conversion
//! is submitted, but no faster than the client's refresh rate.
//!
//! Each encoder lives for one video epoch: one size and one format. A new
//! size, or a new format from negotiation (§3), starts a new epoch, which
//! the client hears about in an `Epoch` message before its first frame.

use std::fs::File;
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::{import_surface, with_renderer_surface_state};
use smithay::backend::renderer::{ExportMem, Renderer, Texture};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Clock, Monotonic, Rectangle};
use smithay::wayland::compositor::with_states;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};
use smithay::wayland::shell::xdg::ToplevelSurface;
use tokio::sync::mpsc::UnboundedSender;

use farsight_proto::codec::{Choice, Encoding, Format, Mode};
use farsight_proto::control::{Epoch, ServerMessage};
use farsight_proto::tiles::Rect;
use smithay::backend::renderer::utils::CommitCounter;

use crate::encode::{self, Encoder, EncoderInfo, FrameKind, Frames, Input, Output, Settings, Shaders};
use crate::host::Host;
use crate::net::{self, ToNet};

/// A layout change in progress. M0 found the change is not one step: frames
/// at the new size arrive with old content, and the scale may land before
/// the size. So nothing is encoded until the nested compositor reports the
/// new size and scale (through `wlr-output-management`) and commits a frame
/// at that size after it, or until the timeout.
struct Hold {
    size: (i32, i32),
    scale: f64,
    since: Instant,
    confirmed: bool,
}

/// How long the picture must stay still before it is refined.
const IDLE: Duration = Duration::from_millis(250);

struct Idle {
    last_change: Instant,
    /// The picture since the last change has been refined (or needs none).
    refined: bool,
    /// The idle timer is armed.
    timer: bool,
}

/// Notes that a new picture went out, and arms the idle timer.
fn changed(host: &mut Host) {
    let p = &mut host.pipeline;
    p.idle.last_change = Instant::now();
    p.idle.refined = p.opts.refine_qp >= p.opts.qp && !matches!(p.frames, Some((_, Encoding::Tiles)));
    if !p.idle.timer && !p.idle.refined {
        p.idle.timer = true;
        let _ = host.loop_handle.insert_source(Timer::from_duration(IDLE), |_, _, host| idle_tick(host));
    }
}

fn idle_tick(host: &mut Host) -> TimeoutAction {
    let p = &mut host.pipeline;
    if p.idle.refined || !p.watched {
        p.idle.timer = false;
        return TimeoutAction::Drop;
    }
    let still = p.idle.last_change.elapsed();
    if still < IDLE {
        return TimeoutAction::ToDuration(IDLE - still);
    }
    if p.shared.in_flight.load(Ordering::Acquire) > 0 {
        return TimeoutAction::ToDuration(Duration::from_millis(10));
    }
    p.idle.timer = false;
    p.idle.refined = true;
    tracing::debug!("idle: refining");
    encode_current(host, true);
    TimeoutAction::Drop
}

/// The longest a layout change holds frames back.
pub const HOLD_TIMEOUT: Duration = Duration::from_millis(500);

/// A hold's timer ran out: if the nested compositor hasn't drawn the new
/// layout by now, send what it has.
pub fn hold_timed_out(host: &mut Host) {
    if host.pipeline.hold.as_ref().is_some_and(|h| h.since.elapsed() >= HOLD_TIMEOUT) {
        tracing::warn!("the layout change timed out; sending frames again");
        host.pipeline.hold = None;
        refresh(host);
    }
}

/// The nested compositor reported its outputs: does that confirm the
/// layout being waited for?
pub fn outputs_changed(host: &mut Host) {
    let current = host.outputs.as_ref().and_then(|o| o.current());
    if let (Some(hold), Some(cur)) = (host.pipeline.hold.as_mut(), current)
        && cur.size == hold.size
        && (cur.scale - hold.scale).abs() < 1e-3
        && !hold.confirmed
    {
        tracing::debug!(after_ms = hold.since.elapsed().as_millis() as u64, "the nested compositor took the layout");
        hold.confirmed = true;
    }
}

/// Whether a frame of `size` may be encoded, ending the hold if it may.
fn release_hold(host: &mut Host, size: (i32, i32)) -> bool {
    let confirmable_without = host.outputs.is_none();
    let Some(hold) = host.pipeline.hold.as_mut() else { return true };
    let elapsed = hold.since.elapsed();
    if elapsed >= HOLD_TIMEOUT {
        tracing::warn!(?size, want = ?hold.size, scale = hold.scale, "the layout change timed out; sending frames again");
    } else if size != hold.size || !(hold.confirmed || confirmable_without) {
        return false;
    } else {
        tracing::info!(held_ms = elapsed.as_millis() as u64, ?size, scale = hold.scale, "layout applied");
    }
    host.pipeline.hold = None;
    true
}

/// The largest screen sent as tiles.
const TILES_MAX: u32 = 8192;

/// Frames waiting for the encode thread. Beyond this the newest is dropped
/// before encoding, which costs nothing: each frame is a whole picture.
const ENCODE_QUEUE: usize = 2;

pub struct Options {
    pub render_node: PathBuf,
    pub out: Option<PathBuf>,
    pub qp: u32,
    /// JPEG quality for tiles.
    pub jpeg_quality: u8,
    /// QP for idle refinement; the same as `qp` turns it off.
    pub refine_qp: u32,
    /// Read the probe client's frame number back from each nested frame.
    pub probe: bool,
    /// The bitrate to keep video within, from congestion control.
    pub video_target: Arc<AtomicU64>,
    /// When the network's video queue drains, in host µs.
    pub video_drain_at: Arc<AtomicU64>,
    /// How far the clients' decoders are behind.
    pub decoding: Arc<crate::window::Decoding>,
}

/// No RFI answered yet, in [`Shared::answered`].
const NONE: u64 = u64::MAX;

/// A new frame isn't encoded while the network would still hold more than
/// this much video by the time it is ready. Skipping a frame before it is
/// encoded costs nothing; dropping one after would break the stream.
const MAX_QUEUED_US: u64 = 20_000;

pub struct Pipeline {
    opts: Options,
    /// What this machine can encode, best backend first.
    encoders: Vec<EncoderInfo>,
    /// The formats the client and we share, best first (§3). The first one
    /// that fits the picture is used.
    choices: Vec<Choice>,
    shaders: Option<Shaders>,
    /// This epoch's conversion, and its encoding.
    frames: Option<(Frames, Encoding)>,
    /// Damage since the last converted frame, in buffer pixels. Only tiles
    /// use it; video encodes the whole picture.
    damage: Vec<Rect>,
    /// The nested window's commit the damage runs up to.
    last_commit: Option<CommitCounter>,
    /// The client's mode: 4:4:4 JPEG in tiles for text.
    mode: Mode,
    /// Frames are held across a layout change (§5).
    hold: Option<Hold>,
    /// When the picture last changed, for idle refinement (§2).
    idle: Idle,
    /// What the encode thread shares with this one.
    shared: Arc<Shared>,
    /// To the encode thread; `None` only while shutting down.
    jobs: Option<SyncSender<Job>>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    last_buffer: Option<(Fourcc, u64, (i32, i32))>,
    frame_count: u64,
    epoch: u16,
    /// Someone is connected. With no one (and no `--out`), nothing is
    /// converted or encoded.
    watched: bool,
    force_keyframe: bool,
    /// Reference frame invalidation asked for: the newest frame lost and
    /// the oldest good one, of all who asked since the last frame.
    rfi: Option<(u32, u32)>,
    /// When frame callbacks last went out, in host µs.
    last_callback_us: u64,
    callback_timer: bool,
    /// A frame was skipped for the network or a client's decoder (and
    /// whether it was a refinement); it is encoded once they catch up.
    skipped: Option<bool>,
    /// Runs [`resume_skipped`] when they should have, at the latest.
    resume_timer: Option<RegistrationToken>,
    clock: Clock<Monotonic>,
    presented: u64,
}

/// What the main thread and the encode thread share.
struct Shared {
    /// Tiles sent lossy since the last refinement, from the encode thread.
    lossy: Mutex<Vec<Rect>>,
    /// Jobs queued or being encoded: refinement waits for them, so it sees
    /// every lossy tile.
    in_flight: AtomicUsize,
    /// The last frame before the latest RFI answer, from the encode thread
    /// ([`NONE`] before any): an RFI for no frame after it is a repeat.
    answered: AtomicU64,
    /// The bitrate to keep video within.
    video_target: Arc<AtomicU64>,
}

enum Job {
    /// A new epoch: frames after this use this encoder.
    Start(Encoder),
    Encode(Box<EncodeJob>),
}

struct EncodeJob {
    input: Input,
    sync: SyncPoint,
    /// To the network, as well as to `--out`.
    send: bool,
    kind: FrameKind,
    rfi: Option<(u32, u32)>,
    epoch: u16,
    n: u64,
    probe: Option<u32>,
    t_commit: u64,
    t_start: u64,
    t_probe: u64,
    t_surface: u64,
}

impl Pipeline {
    pub fn new(
        opts: Options,
        encoders: Vec<EncoderInfo>,
        start: Instant,
        net: UnboundedSender<ToNet>,
    ) -> anyhow::Result<Self> {
        // With no client, `--out` gets the best encoder.
        let choices = if encoders.is_empty() { Vec::new() } else { vec![choice(&encoders, 0)] };
        let out = match &opts.out {
            Some(p) => Some(File::create(p).with_context(|| format!("creating {}", p.display()))?),
            None => None,
        };
        let (jobs, rx) = mpsc::sync_channel(ENCODE_QUEUE);
        let shared = Arc::new(Shared {
            lossy: Mutex::default(),
            in_flight: AtomicUsize::new(0),
            answered: AtomicU64::new(NONE),
            video_target: opts.video_target.clone(),
        });
        let thread_shared = shared.clone();
        let qp = encode::rate::QpControl::new(51u32.saturating_sub(opts.qp));
        let thread = std::thread::Builder::new()
            .name("farsight-encode".into())
            .spawn(move || encode_thread(rx, out, start, net, thread_shared, qp))?;
        Ok(Self {
            opts,
            encoders,
            choices,
            shaders: None,
            frames: None,
            damage: Vec::new(),
            last_commit: None,
            mode: Mode::default(),
            hold: None,
            idle: Idle { last_change: Instant::now(), refined: true, timer: false },
            shared,
            jobs: Some(jobs),
            encode_thread: Some(thread),
            last_buffer: None,
            frame_count: 0,
            epoch: 0,
            watched: false,
            force_keyframe: false,
            rfi: None,
            last_callback_us: 0,
            callback_timer: false,
            skipped: None,
            resume_timer: None,
            clock: Clock::new(),
            presented: 0,
        })
    }

    /// The output is changing to `layout`: hold the last good frame until
    /// the nested compositor has taken it on and drawn a frame at it.
    pub fn hold(&mut self, layout: crate::host::Layout) {
        self.hold = Some(Hold {
            size: (layout.width, layout.height),
            scale: layout.scale,
            since: Instant::now(),
            confirmed: false,
        });
    }

    /// A new nested window (the desktop restarted): its commits and damage
    /// start afresh, in a new epoch.
    pub fn nested_window_changed(&mut self) {
        self.last_commit = None;
        self.damage.clear();
        self.frames = None;
        self.hold = None;
    }

    /// Whether anyone is connected; frames go to them, starting with a
    /// keyframe.
    pub fn set_watched(&mut self, watched: bool) {
        self.watched = watched;
        self.force_keyframe = true;
    }

    pub fn encoders(&self) -> &[EncoderInfo] {
        &self.encoders
    }

    /// The formats a new client shares with us, best first, and its mode.
    /// The next frame starts a new epoch in the first that fits, or in
    /// tiles if none does.
    pub fn set_choices(&mut self, choices: Vec<Choice>, mode: Mode) {
        self.choices = choices;
        self.mode = mode;
        self.frames = None;
    }

    /// The encodings on offer, best first: the formats, then tiles.
    pub fn encodings(&self) -> Vec<Encoding> {
        self.choices.iter().map(|c| Encoding::Video(c.format)).chain([Encoding::Tiles]).collect()
    }

    /// The client can't decode `format` any more; move on to the next, or
    /// to tiles.
    pub fn drop_format(&mut self, format: Format) {
        self.choices.retain(|c| c.format != format);
        if self.frames.as_ref().is_some_and(|(_, e)| *e == Encoding::Video(format)) {
            self.frames = None;
        }
    }

    /// The largest picture any video format allows, or tiles if there is
    /// none. A size past the first format's limits starts the next epoch
    /// in a format that fits (§3).
    pub fn max_size(&self) -> (u32, u32) {
        if self.choices.is_empty() {
            return (TILES_MAX, TILES_MAX);
        }
        let w = self.choices.iter().map(|c| c.max_width).max().unwrap_or(0);
        let h = self.choices.iter().map(|c| c.max_height).max().unwrap_or(0);
        (w, h)
    }

    fn encoding(&self) -> bool {
        self.watched || self.opts.out.is_some()
    }

    fn send(&self, job: Job) -> Result<(), TrySendError<Job>> {
        self.jobs.as_ref().expect("pipeline running").try_send(job)
    }
}

impl Drop for Pipeline {
    /// Ends the encode thread and waits for it, so its encoder is gone
    /// before the process exits: the iHD driver's exit-time destructors
    /// crash while a VA context is still open.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
        }
    }
}

pub fn on_toplevel_commit(host: &mut Host, toplevel: &ToplevelSurface) {
    let t_commit = host.now_us();
    let surface = toplevel.wl_surface();
    let Some(texture) = nested_texture(host, surface) else {
        return; // null buffer: the nested window unmapped
    };
    log_buffer_change(host, surface);
    collect_damage(host, surface);

    let size = texture.size();
    if !host.initial_configure_repeated {
        host.initial_configure_repeated = true;
        if (size.w, size.h) != (host.layout.width, host.layout.height) {
            tracing::info!(got = ?(size.w, size.h), "first nested frame has the wrong size; repeating configure");
            toplevel.send_configure();
        }
    }

    if host.pipeline.encoding()
        && release_hold(host, (size.w, size.h))
        && let Err(err) = convert_and_encode(host, &texture, t_commit, false)
    {
        tracing::error!("{err:#}");
    }
    present(host, surface);
}

/// Kiosk mode: the app committed; composite it and encode the result.
pub fn on_kiosk_commit(host: &mut Host) {
    let t_commit = host.now_us();
    let (size, scale) = ((host.layout.width, host.layout.height), host.layout.scale);
    let Some(kiosk) = host.kiosk.as_mut() else { return };
    let composited = match kiosk.composite(&mut host.renderer, size, scale) {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(err) => {
            tracing::error!("{err:#}");
            return;
        }
    };
    let (texture, damage) = composited;
    host.pipeline.damage.extend(damage);
    // Hold while the app hasn't drawn itself at the new size yet.
    let drawn = crate::kiosk::drawn_size(host).map(|(w, h)| {
        let near = |a: i32, b: i32| (a - b).abs() <= 2;
        if near(w, size.0) && near(h, size.1) { size } else { (w, h) }
    });
    if host.pipeline.encoding()
        && release_hold(host, drawn.unwrap_or(size))
        && let Err(err) = convert_and_encode(host, &texture, t_commit, false)
    {
        tracing::error!("{err:#}");
    }
    if let Some(surface) = host.toplevel.as_ref().map(|t| t.wl_surface().clone()) {
        present(host, &surface);
    }
}

/// Adds the nested window's damage since the last commit we saw.
fn collect_damage(host: &mut Host, surface: &WlSurface) {
    let p = &mut host.pipeline;
    let commit = with_renderer_surface_state(surface, |rs| {
        let damage = rs.damage_since(p.last_commit);
        for r in damage.iter() {
            let (x, y) = (r.loc.x.max(0), r.loc.y.max(0));
            let (w, h) = ((r.loc.x + r.size.w - x).max(0), (r.loc.y + r.size.h - y).max(0));
            p.damage.push(Rect::new(x as u16, y as u16, w as u16, h as u16));
        }
        rs.current_commit()
    });
    p.last_commit = commit;
}

/// Encodes the nested window's current buffer again, as a keyframe: for a
/// client that just connected or lost its reference while the desktop is
/// idle.
pub fn refresh(host: &mut Host) {
    host.pipeline.force_keyframe = true;
    encode_current(host, false);
}

/// A client can't decode frames after `good`, up to `lost`: encode the
/// next frame from `good`, or as a keyframe (RFI, §2).
pub fn recover(host: &mut Host, lost: u32, good: u32) {
    use farsight_proto::video::before;
    let p = &mut host.pipeline;
    let answered = p.shared.answered.load(Ordering::Relaxed);
    if answered != NONE && !before(answered as u32, lost) {
        return; // a repeat; its answer is on the way
    }
    p.rfi = Some(match p.rfi {
        Some((l, g)) => (if before(l, lost) { lost } else { l }, if before(good, g) { good } else { g }),
        None => (lost, good),
    });
    encode_current(host, false);
}

/// Tiles: sends `rects` again, which the client lost.
pub fn repaint(host: &mut Host, rects: &[Rect]) {
    if !host.pipeline.frames.as_ref().is_some_and(|(_, e)| *e == Encoding::Tiles) {
        return;
    }
    host.pipeline.damage.extend_from_slice(rects);
    encode_current(host, false);
}

/// Encodes the nested window's current buffer, or refines it.
fn encode_current(host: &mut Host, refine: bool) {
    if host.pipeline.hold.is_some() {
        return; // the frame after the hold starts a new epoch anyway
    }
    let Some(surface) = host.toplevel.as_ref().map(|t| t.wl_surface().clone()) else { return };
    let ctx = host.renderer.context_id();
    let texture = match &host.kiosk {
        Some(k) => k.texture(),
        None => with_renderer_surface_state(&surface, |rs| rs.texture::<GlesTexture>(ctx).cloned()).flatten(),
    };
    let Some(texture) = texture else { return };
    let t = host.now_us();
    if let Err(err) = convert_and_encode(host, &texture, t, refine) {
        tracing::error!("{err:#}");
    }
}

/// The nested window's current buffer as a texture. A dmabuf is imported
/// once and cached by Smithay; a wl_shm buffer is uploaded. labwc sends shm
/// when it passes a fullscreen shm app's buffer straight through (direct
/// scanout to its Wayland backend).
pub fn nested_texture(host: &mut Host, surface: &WlSurface) -> Option<GlesTexture> {
    if let Err(err) = with_states(surface, |states| import_surface(&mut host.renderer, states)) {
        tracing::warn!(%err, "importing nested buffer");
        return None;
    }
    let ctx = host.renderer.context_id();
    with_renderer_surface_state(surface, |rs| rs.texture::<GlesTexture>(ctx).cloned()).flatten()
}

fn log_buffer_change(host: &mut Host, surface: &WlSurface) {
    let info = with_renderer_surface_state(surface, |rs| {
        rs.buffer().and_then(|b| {
            smithay::wayland::dmabuf::get_dmabuf(b).ok().map(|d| {
                (d.format().code, u64::from(d.format().modifier), (d.size().w, d.size().h))
            })
        })
    })
    .flatten();
    if info != host.pipeline.last_buffer {
        match info {
            Some((fourcc, modifier, size)) => tracing::info!(
                %fourcc, modifier = format!("{modifier:#x}"), ?size, "nested buffer format"
            ),
            None => tracing::info!("nested buffer is not a dmabuf (wl_shm?)"),
        }
        host.pipeline.last_buffer = info;
    }
}

/// The ranked formats from client `decoders` and the `mode` it wants.
pub fn negotiate(encoders: &[EncoderInfo], decoders: &[farsight_proto::codec::DecoderCaps], mode: Mode) -> Vec<Choice> {
    let caps: Vec<_> = encoders.iter().map(|e| e.caps.clone()).collect();
    farsight_proto::codec::rank(&caps, decoders, mode)
}

/// Encoder `i` as a choice of its own, for when there is no client.
fn choice(encoders: &[EncoderInfo], i: usize) -> Choice {
    let c = &encoders[i].caps;
    Choice { format: c.format, max_width: c.max_width, max_height: c.max_height, encoder: i, hardware: c.hardware as u8 }
}

/// Starts a new epoch at `w`×`h`: opens the first format that fits and
/// opens, and tells the client.
fn new_epoch(host: &mut Host, w: i32, h: i32) -> anyhow::Result<()> {
    let layout = farsight_proto::layout::Layout { width_px: w as u32, height_px: h as u32, ..host.wire_layout() };
    let p = &mut host.pipeline;
    p.frames = None;
    let fits: Vec<Choice> = p.choices.iter().copied().filter(|c| c.fits(w as u32, h as u32)).collect();
    let mut opened = None;
    for c in fits.iter().chain(p.choices.first()) {
        let info = &p.encoders[c.encoder];
        let settings = Settings { qp: p.opts.qp, refine_qp: p.opts.refine_qp };
        match encode::open(info, &p.opts.render_node, &mut host.renderer, w, h, &settings) {
            Ok(pair) => {
                opened = Some((pair, c.format));
                break;
            }
            Err(err) => tracing::warn!(format = %c.format, backend = ?info.backend, "{err:#}; trying the next format"),
        }
    }
    let ((frames, encoder), encoding) = match opened {
        Some((pair, format)) => (pair, Encoding::Video(format)),
        None => (encode::open_tiles(w, h)?, Encoding::Tiles),
    };
    // The encode thread takes it in order, after the old epoch's frames.
    let jobs = p.jobs.as_ref().expect("pipeline running");
    jobs.send(Job::Start(encoder)).map_err(|_| anyhow::anyhow!("encode thread gone"))?;
    p.frames = Some((frames, encoding));
    p.shared.lossy.lock().unwrap().clear();
    p.epoch = p.epoch.wrapping_add(1);
    p.force_keyframe = true;
    tracing::info!(epoch = p.epoch, w, h, %encoding, "new video epoch");
    if p.watched {
        let epoch = Epoch { epoch: p.epoch, encoding, layout };
        let _ = host.net.send(ToNet::Broadcast(ServerMessage::Epoch(epoch)));
    }
    Ok(())
}

fn convert_and_encode(host: &mut Host, texture: &GlesTexture, t_commit: u64, refine: bool) -> anyhow::Result<()> {
    let size = texture.size();
    let (w, h) = (size.w, size.h);
    let start = host.start;
    let now = || start.elapsed().as_micros() as u64;

    if host.pipeline.shaders.is_none() {
        host.pipeline.shaders = Some(Shaders::compile(&mut host.renderer)?);
    }
    if host.pipeline.frames.as_ref().is_none_or(|(f, _)| f.size() != (w, h)) {
        // A new size is a new video epoch (§5).
        new_epoch(host, w, h)?;
    }

    if skip_for_backlog(host, refine) {
        return Ok(());
    }

    let t_start = now();
    let probe = if host.pipeline.opts.probe { read_probe(&mut host.renderer, texture)? } else { None };
    let t_probe = now();

    let p = &mut host.pipeline;
    let (frames, encoding) = p.frames.as_mut().unwrap();
    let kind = match (p.force_keyframe, refine) {
        (true, _) => FrameKind::Keyframe,
        (false, true) => FrameKind::Refine,
        (false, false) => FrameKind::Normal,
    };
    // Tiles send the damage, everything for a "keyframe", or what went
    // lossy for a refinement.
    let damage = match (*encoding, kind) {
        (Encoding::Tiles, FrameKind::Keyframe) => vec![Rect::new(0, 0, w as u16, h as u16)],
        (Encoding::Tiles, FrameKind::Refine) => {
            encode::tiles::align(&std::mem::take(&mut *p.shared.lossy.lock().unwrap()), w, h)
        }
        (Encoding::Tiles, FrameKind::Normal) => encode::tiles::align(&p.damage, w, h),
        (Encoding::Video(_), _) => Vec::new(),
    };
    if kind != FrameKind::Refine {
        p.damage.clear();
    } else {
        tracing::debug!(regions = damage.len(), "refinement");
    }
    if *encoding == Encoding::Tiles && damage.is_empty() {
        return Ok(());
    }
    let tile_options = farsight_tiles::Options {
        quality: p.opts.jpeg_quality,
        chroma444: p.mode == Mode::Text,
        lossless: kind == FrameKind::Refine,
    };
    let t_surface = now();
    let (input, sync) = frames.convert(&mut host.renderer, p.shaders.as_ref().unwrap(), texture, &damage, tile_options)?;

    p.frame_count += 1;
    let job = EncodeJob {
        input,
        sync,
        send: p.watched,
        kind,
        rfi: p.rfi,
        epoch: p.epoch,
        n: p.frame_count,
        probe,
        t_commit,
        t_start,
        t_probe,
        t_surface,
    };
    p.shared.in_flight.fetch_add(1, Ordering::AcqRel);
    match p.send(Job::Encode(Box::new(job))) {
        Ok(()) => {
            p.force_keyframe = false;
            p.rfi = None;
            if kind != FrameKind::Refine {
                changed(host);
            }
        }
        Err(TrySendError::Full(_)) => {
            p.shared.in_flight.fetch_sub(1, Ordering::AcqRel);
            tracing::debug!("encoder busy; frame dropped");
            // Tiles must still send what changed.
            match kind {
                FrameKind::Refine => p.shared.lossy.lock().unwrap().extend_from_slice(&damage),
                _ => p.damage.extend_from_slice(&damage),
            }
        }
        Err(TrySendError::Disconnected(_)) => anyhow::bail!("encode thread gone"),
    }
    Ok(())
}

/// Whether to leave this frame out because the network, or a client's
/// decoder, is behind; if so, the current picture is encoded once both
/// have caught up.
fn skip_for_backlog(host: &mut Host, refine: bool) -> bool {
    let p = &mut host.pipeline;
    // A keyframe replaces whatever is queued, and a client waiting to
    // recover can't use what is.
    if p.force_keyframe || p.rfi.is_some() {
        p.skipped = None;
        return false;
    }
    let now = host.start.elapsed().as_micros() as u64;
    let drain_at = p.opts.video_drain_at.load(Ordering::Relaxed);
    let network = (now + MAX_QUEUED_US < drain_at).then(|| drain_at - MAX_QUEUED_US);
    // A decoder that has room sooner says so (`resume_skipped`).
    let decoder = p.opts.decoding.behind(now, p.shared.in_flight.load(Ordering::Acquire));
    let Some(resume_at) = network.max(decoder) else {
        p.skipped = None;
        return false;
    };
    if let Some(timer) = p.resume_timer.take() {
        host.loop_handle.remove(timer);
    }
    let wait = Duration::from_micros(resume_at.saturating_sub(now));
    let timer = host.loop_handle.insert_source(Timer::from_duration(wait), |_, _, host| {
        host.pipeline.resume_timer = None;
        resume_skipped(host);
        TimeoutAction::Drop
    });
    p.resume_timer = timer.ok();
    // The latest wins: a new picture makes a refinement owed moot, and
    // sending it arms the next one.
    p.skipped = Some(refine);
    tracing::debug!(
        network_ms = network.map(|t| (t - now) / 1000),
        decoder = decoder.is_some(),
        "behind; frame skipped"
    );
    true
}

/// Encodes the picture a skip left out, if any, now that what it was
/// skipped for may have caught up.
pub fn resume_skipped(host: &mut Host) {
    if let Some(refine) = host.pipeline.skipped.take() {
        encode_current(host, refine);
    }
}

/// Tells the nested compositor its frame is done: presentation feedback
/// now, frame callbacks now or once a refresh interval has passed since the
/// last ones.
fn present(host: &mut Host, surface: &WlSurface) {
    let p = &mut host.pipeline;
    let interval = Duration::from_micros(1_000_000_000 / host.layout.refresh_mhz.max(1000) as u64);
    let time: Duration = p.clock.now().into();
    p.presented += 1;
    let callbacks = with_states(surface, |s| {
        std::mem::take(&mut s.cached_state.get::<PresentationFeedbackCachedState>().current().callbacks)
    });
    for cb in callbacks {
        cb.presented(&host.output, time, Refresh::fixed(interval), p.presented, wp_presentation_feedback::Kind::empty());
    }

    let since = host.now_us().saturating_sub(host.pipeline.last_callback_us);
    // A little slack for timer wake-up, so a desktop that keeps pace isn't
    // pushed to every other interval.
    let due = (interval.as_micros() as u64).saturating_sub(500);
    if since >= due {
        send_frame_callbacks(host);
    } else if !host.pipeline.callback_timer {
        host.pipeline.callback_timer = true;
        let wait = Duration::from_micros(due - since);
        let _ = host.loop_handle.insert_source(Timer::from_duration(wait), |_, _, host| {
            host.pipeline.callback_timer = false;
            send_frame_callbacks(host);
            TimeoutAction::Drop
        });
    }
}

fn send_frame_callbacks(host: &mut Host) {
    let Some(toplevel) = host.toplevel.as_ref() else { return };
    host.pipeline.last_callback_us = host.now_us();
    let output = host.output.clone();
    let time: Duration = host.pipeline.clock.now().into();
    match host.kiosk.as_ref().and_then(|k| k.window.as_ref()) {
        // The app's popups too.
        Some(window) => window.send_frame(&output, time, None, |_, _| Some(output.clone())),
        None => smithay::desktop::utils::send_frames_surface_tree(toplevel.wl_surface(), &output, time, None, |_, _| {
            Some(output.clone())
        }),
    }
    let _ = host.display.flush_clients();
}

fn encode_thread(
    rx: mpsc::Receiver<Job>,
    mut out: Option<File>,
    start: Instant,
    net: UnboundedSender<ToNet>,
    shared: Arc<Shared>,
    mut qp: encode::rate::QpControl,
) {
    let now = || start.elapsed().as_micros() as u64;
    let mut codec = None;
    // Video frame numbers, across epochs; the first of this encoder's; the
    // last frame before the latest RFI answer.
    let mut next_frame: u32 = 0;
    let mut epoch_first: u32 = 0;
    let mut answered: Option<u32> = None;
    for job in rx {
        let job = match job {
            Job::Start(c) => {
                codec = Some(c);
                epoch_first = next_frame;
                // A new epoch may be another size, or codec.
                if let Some(f) = &mut out
                    && let Err(err) = f.set_len(0).and_then(|()| f.rewind())
                {
                    tracing::error!(%err, "restarting the stream file");
                    out = None;
                }
                continue;
            }
            Job::Encode(job) => job,
        };
        // Counts the job done however this iteration ends.
        struct Done<'a>(&'a AtomicUsize);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _done = Done(&shared.in_flight);
        let Some(codec) = codec.as_mut() else { continue };
        // The conversion pass must have finished writing the surface.
        let _ = job.sync.wait();
        let t_converted = now();
        let number = next_frame;
        let (kind, mut refs) = match job.rfi {
            Some((lost, good)) if codec.is_video() => {
                let answer = rfi_answer(codec, job.kind, number, epoch_first, &mut answered, lost, good);
                shared.answered.store(answered.map_or(NONE, u64::from), Ordering::Relaxed);
                answer
            }
            _ => (job.kind, number.wrapping_sub(1)),
        };
        let qp_offset = match codec.is_video() && job.send {
            true => qp.next(Instant::now(), shared.video_target.load(Ordering::Relaxed)),
            false => 0,
        };
        let output = match codec.encode(job.input, job.t_commit as i64, number, kind, qp_offset) {
            Ok(o) => o,
            Err(err) => {
                tracing::error!("{err:#}");
                continue;
            }
        };
        let t_encoded = now();
        let encode_us = (t_encoded - job.t_commit) as u32;
        let video = matches!(output, Output::Video { .. });
        let (bytes, keyframe) = match output {
            Output::Video { data, keyframe } => {
                next_frame = next_frame.wrapping_add(1);
                if keyframe {
                    refs = number;
                }
                if let Some(f) = &mut out
                    && let Err(err) = f.write_all(&data)
                {
                    tracing::error!(%err, "writing the stream");
                    out = None;
                }
                let bytes = data.len();
                if job.send {
                    let frame = net::Frame {
                        data,
                        keyframe,
                        epoch: job.epoch,
                        number,
                        refs,
                        capture_us: job.t_commit,
                        encode_us,
                    };
                    let _ = net.send(ToNet::Frame(frame));
                }
                (bytes, keyframe)
            }
            Output::Tiles(mut update) => {
                let bytes = update.bodies.iter().map(Vec::len).sum();
                shared.lossy.lock().unwrap().append(&mut update.lossy);
                if job.send {
                    let tiles = net::Tiles { update, epoch: job.epoch, capture_us: job.t_commit, encode_us };
                    let _ = net.send(ToNet::Tiles(tiles));
                }
                (bytes, kind == FrameKind::Keyframe)
            }
        };

        if video && job.send {
            qp.spent(Instant::now(), bytes);
        }

        // One line per frame, parsed by tools/m0/analyze.py.
        tracing::info!(
            target: "frame",
            n = job.n,
            epoch = job.epoch,
            probe = job.probe.map(|s| s as i64).unwrap_or(-1),
            t_commit_mono_us = mono_us(start, job.t_commit),
            commit_to_surface_us = job.t_surface - job.t_commit,
            import_us = job.t_start - job.t_commit,
            probe_readback_us = job.t_probe - job.t_start,
            convert_us = t_converted - job.t_surface,
            encode_us = t_encoded - t_converted,
            total_us = t_encoded - job.t_commit,
            bytes,
            keyframe,
            refine = kind == FrameKind::Refine,
            qp_offset,
            frame = video.then_some(number),
            refs = (video && refs != number.wrapping_sub(1)).then_some(refs),
            "frame"
        );
    }
}

/// How frame `number` answers an RFI for `good..=lost`: as `kind` from
/// `good`, if the encoder still holds it, or as a keyframe. A repeat of
/// one already answered is ignored. Returns the kind and the newest frame
/// referenced.
fn rfi_answer(
    codec: &mut Encoder,
    kind: FrameKind,
    number: u32,
    epoch_first: u32,
    answered: &mut Option<u32>,
    lost: u32,
    good: u32,
) -> (FrameKind, u32) {
    use farsight_proto::video::before;
    let previous = number.wrapping_sub(1);
    if answered.is_some_and(|a| !before(a, lost)) {
        tracing::debug!(lost, good, "RFI answered already");
        return (kind, previous);
    }
    *answered = Some(previous);
    // `good` must be this encoder's, and earlier than this frame.
    let ours = !before(good, epoch_first) && before(good, number);
    if kind != FrameKind::Keyframe && ours && codec.invalidate(good.wrapping_add(1), previous) {
        tracing::debug!(lost, good, number, "RFI: predicting from the last good frame");
        (kind, good)
    } else {
        tracing::debug!(lost, good, number, "RFI: keyframe");
        (FrameKind::Keyframe, number)
    }
}

/// The probe client paints its frame number over most of its window
/// (R = low byte, G = high byte); labwc centres the window, so read the
/// middle pixel.
fn read_probe(renderer: &mut GlesRenderer, texture: &GlesTexture) -> anyhow::Result<Option<u32>> {
    let size = texture.size();
    let centre = Rectangle::new((size.w / 2, size.h / 2).into(), (1, 1).into());
    let mapping = renderer.copy_texture(texture, centre, Fourcc::Abgr8888)?;
    let px = renderer.map_texture(&mapping)?;
    if px.len() < 4 {
        return Ok(None);
    }
    // Abgr8888 is R, G, B, A in memory. The probe sets B = 0xA5 as a marker.
    if px[2] != 0xA5 {
        return Ok(None);
    }
    Ok(Some(px[0] as u32 | (px[1] as u32) << 8))
}

/// Convert a host-relative timestamp to CLOCK_MONOTONIC, which the probe
/// client logs in.
fn mono_us(start: Instant, rel_us: u64) -> u64 {
    let now_rel = start.elapsed().as_micros() as i64;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let now_mono = ts.tv_sec * 1_000_000 + ts.tv_nsec / 1000;
    (now_mono - (now_rel - rel_us as i64)) as u64
}
