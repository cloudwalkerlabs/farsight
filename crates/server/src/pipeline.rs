//! Per-frame work (docs/design.md §2, steps 1–5): nested buffer → RGB→NV12
//! shader passes on the main thread → encoder on the encode thread → the
//! network, and optionally a file.
//!
//! The main thread only submits the conversion. The encode thread waits for
//! its fence, encodes and hands the packet on, so the event loop is free for
//! the nested compositor's next frame. Frame callbacks go out as soon as the
//! conversion is submitted, but no faster than the client's refresh rate.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::Context;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::{import_surface, with_renderer_surface_state};
use smithay::backend::renderer::{Bind, ExportMem, Frame, Renderer, Texture};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Clock, Monotonic, Rectangle, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};
use smithay::wayland::shell::xdg::ToplevelSurface;
use tokio::sync::mpsc::UnboundedSender;

use crate::encode::{Codec, Surface, Surfaces};
use crate::host::Host;
use crate::net::{self, ConnId, ToNet};

const SHADER_HEAD: &str = r#"#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
"#;

// BT.709, limited range.
const LUMA_MAIN: &str = r#"
void main() {
    vec3 rgb = texture2D(tex, v_coords).rgb;
    float y = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    gl_FragColor = vec4(16.0 / 255.0 + y * 219.0 / 255.0, 0.0, 0.0, 1.0);
}
"#;

// Rendered at half size: bilinear sampling at the centre of each 2×2 block
// averages it.
const CHROMA_MAIN: &str = r#"
void main() {
    vec3 rgb = texture2D(tex, v_coords).rgb;
    float y = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    float cb = (rgb.b - y) / 1.8556;
    float cr = (rgb.r - y) / 1.5748;
    gl_FragColor = vec4(0.5 + cb * 224.0 / 255.0, 0.5 + cr * 224.0 / 255.0, 0.0, 1.0);
}
"#;

/// Frames waiting for the encode thread. Beyond this the newest is dropped
/// before encoding, which costs nothing: each frame is a whole picture.
const ENCODE_QUEUE: usize = 2;

pub struct Options {
    pub render_node: PathBuf,
    pub out: Option<PathBuf>,
    pub qp: u32,
    /// Read the probe client's frame number back from each nested frame.
    pub probe: bool,
}

pub struct Pipeline {
    opts: Options,
    luma: Option<GlesTexProgram>,
    chroma: Option<GlesTexProgram>,
    surfaces: Option<Surfaces>,
    /// To the encode thread; `None` only while shutting down.
    jobs: Option<SyncSender<Job>>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    last_buffer: Option<(Fourcc, u64, (i32, i32))>,
    frames: u64,
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
    Start(Codec),
    Encode(EncodeJob),
}

struct EncodeJob {
    surface: Surface,
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
    pub fn new(opts: Options, start: Instant, net: UnboundedSender<ToNet>) -> anyhow::Result<Self> {
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
            luma: None,
            chroma: None,
            surfaces: None,
            jobs: Some(jobs),
            encode_thread: Some(thread),
            last_buffer: None,
            frames: 0,
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

/// Encodes the nested window's current buffer again, as a keyframe: for a
/// client that just connected or lost its reference while the desktop is
/// idle.
pub fn refresh(host: &mut Host) {
    host.pipeline.force_keyframe = true;
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

fn convert_and_encode(host: &mut Host, texture: &GlesTexture, t_commit: u64) -> anyhow::Result<()> {
    let size = texture.size();
    let (w, h) = (size.w, size.h);
    let start = host.start;
    let now = || start.elapsed().as_micros() as u64;
    let p = &mut host.pipeline;

    if p.luma.is_none() {
        p.luma = Some(host.renderer.compile_custom_texture_shader(format!("{SHADER_HEAD}{LUMA_MAIN}"), &[])?);
        p.chroma = Some(host.renderer.compile_custom_texture_shader(format!("{SHADER_HEAD}{CHROMA_MAIN}"), &[])?);
    }
    if p.surfaces.as_ref().is_none_or(|s| (s.width, s.height) != (w, h)) {
        // A new size is a new video epoch (§5).
        p.surfaces = None;
        let (surfaces, codec) = Surfaces::new(&p.opts.render_node, w, h, p.opts.qp)?;
        p.surfaces = Some(surfaces);
        // The encode thread takes it in order, after the old epoch's frames.
        let jobs = p.jobs.as_ref().expect("pipeline running");
        jobs.send(Job::Start(codec)).map_err(|_| anyhow::anyhow!("encode thread gone"))?;
        p.epoch = p.epoch.wrapping_add(1);
        p.force_keyframe = true;
        tracing::info!(epoch = p.epoch, w, h, "new video epoch");
    }

    let t_start = now();
    let probe = if p.opts.probe { read_probe(&mut host.renderer, texture)? } else { None };
    let t_probe = now();

    let (surface, mut target) = p.surfaces.as_mut().unwrap().next_surface()?;
    let t_surface = now();

    let luma_prog = p.luma.clone().unwrap();
    let chroma_prog = p.chroma.clone().unwrap();
    let src = Rectangle::from_size(size.to_f64());
    let mut sync = SyncPoint::signaled();
    for (dmabuf, prog, (tw, th)) in [
        (&mut target.luma, &luma_prog, (w, h)),
        (&mut target.chroma, &chroma_prog, ((w + 1) / 2, (h + 1) / 2)),
    ] {
        let mut fb = host.renderer.bind(dmabuf)?;
        let dst = Rectangle::from_size((tw, th).into());
        let mut frame = host.renderer.render(&mut fb, (tw, th).into(), Transform::Normal)?;
        frame.render_texture_from_to(texture, src, dst, &[dst], &[dst], Transform::Normal, 1.0, Some(prog), &[])?;
        sync = frame.finish()?;
    }

    let p = &mut host.pipeline;
    p.frames += 1;
    let job = EncodeJob {
        surface,
        sync,
        client: p.client,
        force_keyframe: p.force_keyframe,
        epoch: p.epoch,
        n: p.frames,
        probe,
        t_commit,
        t_start,
        t_probe,
        t_surface,
    };
    match p.send(Job::Encode(job)) {
        Ok(()) => p.force_keyframe = false,
        Err(TrySendError::Full(_)) => tracing::debug!("encoder busy; frame dropped"),
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
    let mut packet = Vec::new();
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
        packet.clear();
        let keyframe = match codec.encode(job.surface, job.t_commit as i64, job.force_keyframe, &mut packet) {
            Ok(k) => k,
            Err(err) => {
                tracing::error!("{err:#}");
                continue;
            }
        };
        let t_encoded = now();
        if let Some(f) = &mut out
            && let Err(err) = f.write_all(&packet)
        {
            tracing::error!(%err, "writing the stream");
            out = None;
        }
        if let Some(client) = job.client {
            let frame = net::Frame {
                data: packet.clone(),
                keyframe,
                epoch: job.epoch,
                capture_us: job.t_commit,
                encode_us: (t_encoded - job.t_commit) as u32,
            };
            let _ = net.send(ToNet::Frame(client, frame));
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
            bytes = packet.len(),
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
