//! Hardware encoders (docs/design.md §2, §3): what the server can encode,
//! and one encoder per video epoch. There is no software video encoding:
//! without a hardware encoder the server sends tiles instead.
//!
//! Each backend has two halves. [`Frames`] stays on the main thread with the
//! renderer and converts the nested window's buffer into the encoder's input;
//! [`Encoder`] runs on the encode thread.
//!
//! - **VA-API**: the conversion renders straight into the encoder's surfaces
//!   (zero copy).
//! - **NVENC**: the conversion is read back into memory and copied to the
//!   encoder, since the host usually renders on another GPU.
//! - **Tiles**, with no hardware encoder: the damage is read back and coded
//!   on the CPU.

pub mod ffmpeg;
mod nvenc;
mod readback;
pub mod tiles;
mod vaapi;

use std::path::Path;
use std::str::FromStr;

use farsight_proto::codec::{Chroma, EncoderCaps};
use farsight_proto::tiles::Rect;
use ffmpeg_sys_next as ff;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    Nvenc,
}

impl FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "vaapi" => Ok(Backend::Vaapi),
            "nvenc" => Ok(Backend::Nvenc),
            _ => Err(format!("unknown encoder backend {s:?}: expected vaapi or nvenc")),
        }
    }
}

/// One encoder the server can open.
#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub caps: EncoderCaps,
    pub backend: Backend,
    /// VA-API only: the driver encodes this format only in low-power mode.
    pub low_power: bool,
}

/// Every encoder this machine has among `backends`, in that order.
pub fn probe(render_node: &Path, backends: &[Backend]) -> Vec<EncoderInfo> {
    let mut out = Vec::new();
    for &backend in backends {
        match backend {
            Backend::Vaapi => {
                for s in farsight_va::query(render_node, farsight_va::Direction::Encode) {
                    // The conversion writes NV12; 4:4:4 surfaces are packed
                    // formats that it doesn't write yet.
                    if s.format.chroma != Chroma::Yuv420 || !ffmpeg_has(vaapi::encoder_name(s.format.codec)) {
                        continue;
                    }
                    out.push(EncoderInfo {
                        caps: EncoderCaps {
                            format: s.format,
                            max_width: s.max_width,
                            max_height: s.max_height,
                            hardware: true,
                        },
                        backend,
                        low_power: s.low_power,
                    });
                }
            }
            Backend::Nvenc => {
                for caps in nvenc::probe() {
                    out.push(EncoderInfo { caps, backend, low_power: false });
                }
            }
        }
    }
    out
}

fn ffmpeg_has(name: &std::ffi::CStr) -> bool {
    // SAFETY: a lookup by NUL-terminated name.
    !unsafe { ff::avcodec_find_encoder_by_name(name.as_ptr()) }.is_null()
}

/// What every encoder is opened with.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Constant QP for ordinary frames (H.264's scale; AV1 scales it).
    pub qp: u32,
    /// QP for idle refinement (§2): the picture sent again once it stops
    /// changing.
    pub refine_qp: u32,
}

/// How a picture in memory is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneLayout {
    /// Y, then U and V interleaved at half size.
    Nv12,
    /// Y, U and V at full size.
    Yuv444,
}

impl PlaneLayout {
    /// Bytes per row and rows of plane `i`.
    pub fn plane_size(self, i: usize, width: usize, height: usize) -> (usize, usize) {
        match (self, i) {
            (_, 0) | (PlaneLayout::Yuv444, _) => (width, height),
            (PlaneLayout::Nv12, _) => (2 * width.div_ceil(2), height.div_ceil(2)),
        }
    }
}

/// A converted picture in memory.
pub struct MemFrame {
    pub layout: PlaneLayout,
    pub planes: Vec<Vec<u8>>,
    /// Bytes per row of each plane.
    pub strides: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Normal,
    /// A keyframe the decoder can start from (an IDR).
    Keyframe,
    /// The same picture again at `Settings::refine_qp`, once the screen is
    /// idle; tiles send their lossy tiles again losslessly.
    Refine,
}

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

/// The conversion shaders, compiled once.
pub struct Shaders {
    /// NV12 into VA-API surfaces, one pass per plane.
    pub luma: GlesTexProgram,
    pub chroma: GlesTexProgram,
    /// Any layout, packed for read back.
    pub pack: GlesTexProgram,
}

impl Shaders {
    pub fn compile(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        Ok(Self {
            luma: renderer.compile_custom_texture_shader(format!("{SHADER_HEAD}{LUMA_MAIN}"), &[])?,
            chroma: renderer.compile_custom_texture_shader(format!("{SHADER_HEAD}{CHROMA_MAIN}"), &[])?,
            pack: readback::compile(renderer, SHADER_HEAD)?,
        })
    }
}

/// The main thread's half: converts into the encoder's input.
pub enum Frames {
    Va(vaapi::Surfaces),
    Mem(readback::Readback),
    Tiles(tiles::Screen),
}

/// One converted picture, on its way to the encode thread.
pub enum Input {
    Va(vaapi::Surface),
    Mem(MemFrame),
    Tiles(tiles::Damage),
}

/// The encode thread's half.
pub enum Encoder {
    Ffmpeg(ffmpeg::FfEncoder),
    Nvenc(Box<nvenc::Nvenc>),
    Tiles(tiles::TileEncoder),
}

/// What one picture encodes to.
pub enum Output {
    Video { data: Vec<u8>, keyframe: bool },
    Tiles(tiles::Update),
}

/// Tiles for a `width`×`height` screen: one epoch.
pub fn open_tiles(width: i32, height: i32) -> anyhow::Result<(Frames, Encoder)> {
    Ok((Frames::Tiles(tiles::Screen { width, height }), Encoder::Tiles(tiles::TileEncoder::new()?)))
}

/// Opens `info`'s encoder for one size: one video epoch.
pub fn open(
    info: &EncoderInfo,
    render_node: &Path,
    renderer: &mut GlesRenderer,
    width: i32,
    height: i32,
    settings: &Settings,
) -> anyhow::Result<(Frames, Encoder)> {
    match info.backend {
        Backend::Vaapi => {
            let (surfaces, codec) = vaapi::open(info, render_node, width, height, settings)?;
            let codec = codec.with_refinement(settings.qp, settings.refine_qp, vaapi::quant_range(info.caps.format.codec));
            Ok((Frames::Va(surfaces), Encoder::Ffmpeg(codec)))
        }
        Backend::Nvenc => {
            let format = info.caps.format;
            let codec = nvenc::Nvenc::open(format, width, height, settings)?;
            let readback = readback::Readback::new(renderer, width, height, nvenc::plane_layout(format))?;
            Ok((Frames::Mem(readback), Encoder::Nvenc(Box::new(codec))))
        }
    }
}

impl Frames {
    pub fn size(&self) -> (i32, i32) {
        match self {
            Frames::Va(s) => (s.width, s.height),
            Frames::Mem(r) => (r.width, r.height),
            Frames::Tiles(s) => (s.width, s.height),
        }
    }

    /// Converts `texture`. Tiles take only `damage` (aligned with
    /// [`tiles::align`]); video takes the whole picture. The sync point
    /// signals when the input is ready.
    pub fn convert(
        &mut self,
        renderer: &mut GlesRenderer,
        shaders: &Shaders,
        texture: &GlesTexture,
        damage: &[Rect],
        tile_options: farsight_tiles::Options,
    ) -> anyhow::Result<(Input, SyncPoint)> {
        match self {
            Frames::Va(s) => {
                let (surface, sync) = s.convert(renderer, shaders, texture)?;
                Ok((Input::Va(surface), sync))
            }
            Frames::Mem(r) => Ok((Input::Mem(r.convert(renderer, &shaders.pack, texture)?), SyncPoint::signaled())),
            Frames::Tiles(s) => {
                Ok((Input::Tiles(s.read(renderer, texture, damage, tile_options)?), SyncPoint::signaled()))
            }
        }
    }
}

impl Encoder {
    pub fn encode(&mut self, input: Input, pts_us: i64, kind: FrameKind) -> anyhow::Result<Output> {
        match (self, input) {
            (Encoder::Ffmpeg(c), Input::Va(s)) => {
                let mut data = Vec::new();
                // SAFETY: the surface comes from this epoch's VA-API pool.
                let keyframe = unsafe { c.encode(s.into_raw(), pts_us, kind, &mut data)? };
                Ok(Output::Video { data, keyframe })
            }
            (Encoder::Nvenc(c), Input::Mem(f)) => {
                let mut data = Vec::new();
                let keyframe = c.encode(f, pts_us, kind, &mut data)?;
                Ok(Output::Video { data, keyframe })
            }
            (Encoder::Tiles(t), Input::Tiles(d)) => Ok(Output::Tiles(t.encode(d)?)),
            _ => anyhow::bail!("input from another epoch's backend"),
        }
    }
}
