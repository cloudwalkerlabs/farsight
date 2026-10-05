//! The conversion for encoders that take pictures in memory (NVENC, across
//! GPUs): RGB→YUV on the GPU, then a read back.
//!
//! The renderer only reads back RGBA, so each pass packs four bytes of one
//! plane into every RGBA texel. A plane row of `n` bytes is a texture row of
//! `ceil(n / 4)` texels, and what is read back is the plane itself, with a
//! stride of four times that.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType};
use smithay::backend::renderer::{Bind, ExportMem, Frame, Offscreen, Renderer, Texture};
use smithay::utils::{Rectangle, Transform};

use super::{MemFrame, PlaneLayout};

/// Samples the source at plane coordinates and packs four bytes per texel.
/// `kind`: 0 = Y, 1 = U, 2 = V (planar), 3 = U and V interleaved (NV12).
/// `sub`: 2 when the chroma is subsampled 2×2, else 1.
const PACK_MAIN: &str = r#"
uniform vec2 src_size;
uniform vec2 out_size;
uniform float kind;
uniform float sub;

// BT.709, limited range, as the VA-API path.
float sample_plane(float x, float y, float plane) {
    // At the centre of a 2×2 block, bilinear sampling averages it.
    vec2 p = sub > 1.5 ? vec2(2.0 * x + 1.0, 2.0 * y + 1.0) : vec2(x + 0.5, y + 0.5);
    vec3 rgb = texture2D(tex, p / src_size).rgb;
    float l = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    if (plane < 0.5) return 16.0 / 255.0 + l * 219.0 / 255.0;
    if (plane < 1.5) return 0.5 + (rgb.b - l) / 1.8556 * 224.0 / 255.0;
    return 0.5 + (rgb.r - l) / 1.5748 * 224.0 / 255.0;
}

void main() {
    vec2 o = floor(v_coords * out_size);
    if (kind < 2.5) {
        float x = 4.0 * o.x;
        gl_FragColor = vec4(
            sample_plane(x, o.y, kind), sample_plane(x + 1.0, o.y, kind),
            sample_plane(x + 2.0, o.y, kind), sample_plane(x + 3.0, o.y, kind));
    } else {
        float x = 2.0 * o.x;
        gl_FragColor = vec4(
            sample_plane(x, o.y, 1.0), sample_plane(x, o.y, 2.0),
            sample_plane(x + 1.0, o.y, 1.0), sample_plane(x + 1.0, o.y, 2.0));
    }
}
"#;

pub fn compile(renderer: &mut GlesRenderer, head: &str) -> anyhow::Result<GlesTexProgram> {
    let uniforms = [
        UniformName::new("src_size", UniformType::_2f),
        UniformName::new("out_size", UniformType::_2f),
        UniformName::new("kind", UniformType::_1f),
        UniformName::new("sub", UniformType::_1f),
    ];
    Ok(renderer.compile_custom_texture_shader(format!("{head}{PACK_MAIN}"), &uniforms)?)
}

struct Pass {
    texture: GlesTexture,
    /// Texels.
    size: (i32, i32),
    kind: f32,
    sub: f32,
}

pub struct Readback {
    pub width: i32,
    pub height: i32,
    layout: PlaneLayout,
    passes: Vec<Pass>,
}

impl Readback {
    pub fn new(renderer: &mut GlesRenderer, width: i32, height: i32, layout: PlaneLayout) -> anyhow::Result<Self> {
        let (cw, ch) = ((width + 1) / 2, (height + 1) / 2);
        // (bytes per row, rows, kind, sub) for each plane.
        let planes: &[(i32, i32, f32, f32)] = match layout {
            PlaneLayout::Nv12 => &[(width, height, 0.0, 1.0), (2 * cw, ch, 3.0, 2.0)],
            PlaneLayout::Yuv444 => &[(width, height, 0.0, 1.0), (width, height, 1.0, 1.0), (width, height, 2.0, 1.0)],
        };
        let mut passes = Vec::new();
        for &(bytes, rows, kind, sub) in planes {
            let size = ((bytes + 3) / 4, rows);
            let texture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, size.into())?;
            passes.push(Pass { texture, size, kind, sub });
        }
        Ok(Self { width, height, layout, passes })
    }

    /// Converts `texture` and reads it back. Waits for the GPU.
    pub fn convert(
        &mut self,
        renderer: &mut GlesRenderer,
        program: &GlesTexProgram,
        texture: &GlesTexture,
    ) -> anyhow::Result<MemFrame> {
        let size = texture.size();
        let src = Rectangle::from_size(size.to_f64());
        let mut planes = Vec::new();
        let mut strides = Vec::new();
        for pass in &mut self.passes {
            let (tw, th) = pass.size;
            let uniforms = [
                Uniform::new("src_size", (size.w as f32, size.h as f32)),
                Uniform::new("out_size", (tw as f32, th as f32)),
                Uniform::new("kind", pass.kind),
                Uniform::new("sub", pass.sub),
            ];
            let mut fb = renderer.bind(&mut pass.texture)?;
            let dst = Rectangle::from_size((tw, th).into());
            let mut frame = renderer.render(&mut fb, (tw, th).into(), Transform::Normal)?;
            frame.render_texture_from_to(texture, src, dst, &[dst], &[dst], Transform::Normal, 1.0, Some(program), &uniforms)?;
            let _ = frame.finish()?;
            let mapping = renderer.copy_framebuffer(&fb, Rectangle::from_size((tw, th).into()), Fourcc::Abgr8888)?;
            drop(fb);
            planes.push(renderer.map_texture(&mapping)?.to_vec());
            strides.push(4 * tw as usize);
        }
        Ok(MemFrame { layout: self.layout, planes, strides })
    }
}
