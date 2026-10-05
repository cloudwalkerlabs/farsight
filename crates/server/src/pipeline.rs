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
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::Context;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::{import_surface, with_renderer_surface_state};
use smithay::backend::renderer::{ExportMem, Renderer, Texture};
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
use crate::net::{self, ConnId, ToNet};

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
    /// Read the probe client's frame number back from each nested frame.
    pub probe: bool,
}

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
    /// To the encode thread; `None` only while shutting down.
    jobs: Option<SyncSender<Job>>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    last_buffer: Option<(Fourcc, u64, (i32, i32))>,
    frame_count: u64,
    epoch: u16,
    /// The client frames go to. With none (and no `--out`), nothing is
    /// converted or encoded.
    client: Option<ConnId>,
    force_keyframe: bool,
    /// When frame callbacks last went out, in host µs.
    last_callback_us: u64,
    callback_timer: bool,
    clock: Clock<Monotonic>,
    presented: u64,
}

enum Job {
    /// A new epoch: frames after this use this encoder.
    Start(Encoder),
    Encode(Box<EncodeJob>),
}

struct EncodeJob {
    input: Input,
    sync: SyncPoint,
    client: Option<ConnId>,
    force_keyframe: bool,
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
        let thread = std::thread::Builder::new()
            .name("farsight-encode".into())
            .spawn(move || encode_thread(rx, out, start, net))?;
        Ok(Self {
            opts,
            encoders,
            choices,
            shaders: None,
            frames: None,
            damage: Vec::new(),
            last_commit: None,
            mode: Mode::default(),
            jobs: Some(jobs),
            encode_thread: Some(thread),
            last_buffer: None,
            frame_count: 0,
            epoch: 0,
            client: None,
            force_keyframe: false,
            last_callback_us: 0,
            callback_timer: false,
            clock: Clock::new(),
            presented: 0,
        })
    }

    /// Frames go to `client` from now on, starting with a keyframe.
    pub fn set_client(&mut self, client: Option<ConnId>) {
        self.client = client;
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

    /// The largest picture the first encoding allows.
    pub fn max_size(&self) -> (u32, u32) {
        self.choices.first().map_or((TILES_MAX, TILES_MAX), |c| (c.max_width, c.max_height))
    }

    fn encoding(&self) -> bool {
        self.client.is_some() || self.opts.out.is_some()
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
        && let Err(err) = convert_and_encode(host, &texture, t_commit)
    {
        tracing::error!("{err:#}");
    }
    present(host, surface);
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
    encode_current(host);
}

/// Tiles: sends `rects` again, which the client lost.
pub fn repaint(host: &mut Host, rects: &[Rect]) {
    if !host.pipeline.frames.as_ref().is_some_and(|(_, e)| *e == Encoding::Tiles) {
        return;
    }
    host.pipeline.damage.extend_from_slice(rects);
    encode_current(host);
}

/// Encodes the nested window's current buffer.
fn encode_current(host: &mut Host) {
    let Some(surface) = host.toplevel.as_ref().map(|t| t.wl_surface().clone()) else { return };
    let ctx = host.renderer.context_id();
    let Some(texture) = with_renderer_surface_state(&surface, |rs| rs.texture::<GlesTexture>(ctx).cloned()).flatten()
    else {
        return;
    };
    let t = host.now_us();
    if let Err(err) = convert_and_encode(host, &texture, t) {
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
        match encode::open(info, &p.opts.render_node, &mut host.renderer, w, h, &Settings { qp: p.opts.qp }) {
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
    p.epoch = p.epoch.wrapping_add(1);
    p.force_keyframe = true;
    tracing::info!(epoch = p.epoch, w, h, %encoding, "new video epoch");
    if let Some(client) = p.client {
        let epoch = Epoch { epoch: p.epoch, encoding, layout };
        let _ = host.net.send(ToNet::Message(client, ServerMessage::Epoch(epoch)));
    }
    Ok(())
}

fn convert_and_encode(host: &mut Host, texture: &GlesTexture, t_commit: u64) -> anyhow::Result<()> {
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

    let t_start = now();
    let probe = if host.pipeline.opts.probe { read_probe(&mut host.renderer, texture)? } else { None };
    let t_probe = now();

    let p = &mut host.pipeline;
    let (frames, encoding) = p.frames.as_mut().unwrap();
    // Tiles send the damage, or everything for a "keyframe".
    let damage = match *encoding {
        Encoding::Tiles if p.force_keyframe => vec![Rect::new(0, 0, w as u16, h as u16)],
        Encoding::Tiles => encode::tiles::align(&p.damage, w, h),
        Encoding::Video(_) => Vec::new(),
    };
    p.damage.clear();
    if *encoding == Encoding::Tiles && damage.is_empty() {
        return Ok(());
    }
    let tile_options = farsight_tiles::Options {
        quality: p.opts.jpeg_quality,
        chroma444: p.mode == Mode::Text,
        lossless: false,
    };
    let t_surface = now();
    let (input, sync) = frames.convert(&mut host.renderer, p.shaders.as_ref().unwrap(), texture, &damage, tile_options)?;

    p.frame_count += 1;
    let job = EncodeJob {
        input,
        sync,
        client: p.client,
        force_keyframe: p.force_keyframe,
        epoch: p.epoch,
        n: p.frame_count,
        probe,
        t_commit,
        t_start,
        t_probe,
        t_surface,
    };
    match p.send(Job::Encode(Box::new(job))) {
        Ok(()) => p.force_keyframe = false,
        Err(TrySendError::Full(_)) => {
            tracing::debug!("encoder busy; frame dropped");
            // Tiles must still send what changed.
            p.damage.extend_from_slice(&damage);
        }
        Err(TrySendError::Disconnected(_)) => anyhow::bail!("encode thread gone"),
    }
    Ok(())
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
    smithay::desktop::utils::send_frames_surface_tree(toplevel.wl_surface(), &output, time, None, |_, _| {
        Some(output.clone())
    });
    let _ = host.display.flush_clients();
}

fn encode_thread(rx: mpsc::Receiver<Job>, mut out: Option<File>, start: Instant, net: UnboundedSender<ToNet>) {
    let now = || start.elapsed().as_micros() as u64;
    let mut codec = None;
    for job in rx {
        let job = match job {
            Job::Start(c) => {
                codec = Some(c);
                continue;
            }
            Job::Encode(job) => job,
        };
        let Some(codec) = codec.as_mut() else { continue };
        // The conversion pass must have finished writing the surface.
        let _ = job.sync.wait();
        let t_converted = now();
        let kind = if job.force_keyframe { FrameKind::Keyframe } else { FrameKind::Normal };
        let output = match codec.encode(job.input, job.t_commit as i64, kind) {
            Ok(o) => o,
            Err(err) => {
                tracing::error!("{err:#}");
                continue;
            }
        };
        let t_encoded = now();
        let encode_us = (t_encoded - job.t_commit) as u32;
        let (bytes, keyframe) = match output {
            Output::Video { data, keyframe } => {
                if let Some(f) = &mut out
                    && let Err(err) = f.write_all(&data)
                {
                    tracing::error!(%err, "writing the stream");
                    out = None;
                }
                let bytes = data.len();
                if let Some(client) = job.client {
                    let frame = net::Frame { data, keyframe, epoch: job.epoch, capture_us: job.t_commit, encode_us };
                    let _ = net.send(ToNet::Frame(client, frame));
                }
                (bytes, keyframe)
            }
            Output::Tiles(update) => {
                let bytes = update.bodies.iter().map(Vec::len).sum();
                if let Some(client) = job.client {
                    let tiles = net::Tiles { update, epoch: job.epoch, capture_us: job.t_commit, encode_us };
                    let _ = net.send(ToNet::Tiles(client, tiles));
                }
                (bytes, job.force_keyframe)
            }
        };

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
            "frame"
        );
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
