//! Per-frame work: nested buffer → RGB→NV12 shader passes → encoder → file
//! (docs/design.md §2, steps 1–5). Spike instrumentation lives here too.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::utils::{import_surface, with_renderer_surface_state};
use smithay::backend::renderer::{Bind, ExportMem, Frame, Renderer, Texture};
use smithay::utils::{Rectangle, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::encode::Encoder;
use crate::host::Host;

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
    encoder: Option<Encoder>,
    out: Option<File>,
    last_buffer: Option<(Fourcc, u64, (i32, i32))>,
    frames: u64,
    epoch: u32,
    packet: Vec<u8>,
}

impl Pipeline {
    pub fn new(opts: Options) -> anyhow::Result<Self> {
        let out = match &opts.out {
            Some(p) => Some(File::create(p).with_context(|| format!("creating {}", p.display()))?),
            None => None,
        };
        Ok(Self {
            opts,
            luma: None,
            chroma: None,
            encoder: None,
            out,
            last_buffer: None,
            frames: 0,
            epoch: 0,
            packet: Vec::new(),
        })
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

    match convert_and_encode(host, &texture, t_commit) {
        Ok(()) => {}
        Err(err) => tracing::error!("{err:#}"),
    }
}

/// The nested window's current buffer as a texture. A dmabuf is imported
/// once and cached by Smithay; a wl_shm buffer is uploaded. labwc sends shm
/// when it passes a fullscreen shm app's buffer straight through (direct
/// scanout to its Wayland backend).
fn nested_texture(host: &mut Host, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) -> Option<GlesTexture> {
    if let Err(err) = with_states(surface, |states| import_surface(&mut host.renderer, states)) {
        tracing::warn!(%err, "importing nested buffer");
        return None;
    }
    let ctx = host.renderer.context_id();
    with_renderer_surface_state(surface, |rs| rs.texture::<GlesTexture>(ctx).cloned()).flatten()
}

fn log_buffer_change(host: &mut Host, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
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
    if p.encoder.as_ref().is_none_or(|e| (e.width, e.height) != (w, h)) {
        // A new size is a new video epoch (§5).
        p.encoder = None;
        p.encoder = Some(Encoder::new(&p.opts.render_node, w, h, p.opts.qp)?);
        p.epoch += 1;
        tracing::info!(epoch = p.epoch, w, h, "new video epoch");
    }

    let t_start = now();
    let probe_seq = if p.opts.probe { read_probe(&mut host.renderer, texture)? } else { None };
    let t_probe = now();

    let encoder = p.encoder.as_mut().unwrap();
    let (frame, mut target) = encoder.next_surface()?;
    let t_surface = now();

    let luma_prog = p.luma.clone().unwrap();
    let chroma_prog = p.chroma.clone().unwrap();
    let src = Rectangle::from_size(size.to_f64());
    let mut sync = None;
    for (dmabuf, prog, (tw, th)) in [
        (&mut target.luma, &luma_prog, (w, h)),
        (&mut target.chroma, &chroma_prog, ((w + 1) / 2, (h + 1) / 2)),
    ] {
        let mut fb = host.renderer.bind(dmabuf)?;
        let dst = Rectangle::from_size((tw, th).into());
        let mut frame = host.renderer.render(&mut fb, (tw, th).into(), Transform::Normal)?;
        frame.render_texture_from_to(texture, src, dst, &[dst], &[dst], Transform::Normal, 1.0, Some(prog), &[])?;
        sync = Some(frame.finish()?);
    }
    // The spike waits on the CPU; a real pipeline would hand a fence to VA.
    if let Some(sync) = sync {
        let _ = sync.wait();
    }
    let t_converted = now();

    // Release the nested compositor as soon as its buffer has been read (§2 step 3).
    let output = host.output.clone();
    smithay::desktop::utils::send_frames_surface_tree(
        host.toplevel.as_ref().unwrap().wl_surface(),
        &output,
        Duration::from_micros(t_converted),
        None,
        |_, _| Some(output.clone()),
    );
    let _ = host.display.flush_clients();

    let p = &mut host.pipeline;
    p.packet.clear();
    let keyframe = p.encoder.as_mut().unwrap().encode(frame, t_commit as i64, &mut p.packet)?;
    let t_encoded = now();
    if let Some(out) = &mut p.out {
        out.write_all(&p.packet)?;
    }
    p.frames += 1;

    // One line per frame, parsed by the spike's analysis script.
    tracing::info!(
        target: "frame",
        n = p.frames,
        epoch = p.epoch,
        probe = probe_seq.map(|s| s as i64).unwrap_or(-1),
        t_commit_mono_us = mono_us(host.start, t_commit),
        commit_to_surface_us = t_surface - t_commit,
        import_us = t_start - t_commit,
        probe_readback_us = t_probe - t_start,
        convert_us = t_converted - t_surface,
        encode_us = t_encoded - t_converted,
        total_us = t_encoded - t_commit,
        bytes = p.packet.len(),
        keyframe,
        "frame"
    );
    Ok(())
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
fn mono_us(start: std::time::Instant, rel_us: u64) -> u64 {
    let now_rel = start.elapsed().as_micros() as i64;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let now_mono = ts.tv_sec * 1_000_000 + ts.tv_nsec / 1000;
    (now_mono - (now_rel - rel_us as i64)) as u64
}
