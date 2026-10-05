//! Draws decoded pictures into the window with OpenGL ES 3: the planes are
//! uploaded as textures and converted to RGB in the fragment shader
//! (BT.709, limited range, as the server encodes). The picture keeps its
//! aspect ratio, letterboxed in black.

use glow::HasContext;

use crate::decode::{PixelFormat, Picture};

const VERTEX: &str = r#"#version 300 es
out vec2 v_uv;
void main() {
    // A triangle strip over the whole viewport.
    vec2 p = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
precision mediump float;
in vec2 v_uv;
out vec4 color;
uniform sampler2D tex_y;
uniform sampler2D tex_u;
uniform sampler2D tex_v;
uniform int nv12;
void main() {
    float y = texture(tex_y, v_uv).r;
    vec2 uv = nv12 == 1 ? texture(tex_u, v_uv).rg : vec2(texture(tex_u, v_uv).r, texture(tex_v, v_uv).r);
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    uv = (uv - 128.0 / 255.0) * (255.0 / 224.0);
    color = vec4(
        y + 1.5748 * uv.y,
        y - 0.1873 * uv.x - 0.4681 * uv.y,
        y + 1.8556 * uv.x,
        1.0);
}
"#;

/// Where the picture sits in the window, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Picture pixels per window pixel.
    pub scale: f64,
}

impl Placement {
    pub fn fit(window: (u32, u32), picture: (u32, u32)) -> Self {
        let (ww, wh) = (window.0.max(1) as f64, window.1.max(1) as f64);
        let (pw, ph) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
        let s = (ww / pw).min(wh / ph);
        let (w, h) = (pw * s, ph * s);
        Self { x: ((ww - w) / 2.0).round(), y: ((wh - h) / 2.0).round(), width: w, height: h, scale: 1.0 / s }
    }

    /// A window position in picture pixels.
    pub fn to_picture(self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.x) * self.scale, (y - self.y) * self.scale)
    }
}

pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    textures: [glow::Texture; 3],
    nv12: glow::UniformLocation,
    /// The size of what is uploaded, if anything.
    picture: Option<(u32, u32)>,
}

impl Renderer {
    pub fn new(gl: glow::Context) -> anyhow::Result<Self> {
        // SAFETY: the context is current on this thread for the renderer's
        // whole life.
        unsafe {
            let program = gl.create_program().map_err(anyhow::Error::msg)?;
            for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
                let shader = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
                gl.shader_source(shader, src);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    anyhow::bail!("shader: {}", gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                anyhow::bail!("program: {}", gl.get_program_info_log(program));
            }
            gl.use_program(Some(program));
            for (i, name) in ["tex_y", "tex_u", "tex_v"].iter().enumerate() {
                gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), i as i32);
            }
            let nv12 = gl.get_uniform_location(program, "nv12").ok_or_else(|| anyhow::anyhow!("no nv12 uniform"))?;
            let vao = gl.create_vertex_array().map_err(anyhow::Error::msg)?;
            let mut textures = Vec::new();
            for _ in 0..3 {
                let t = gl.create_texture().map_err(anyhow::Error::msg)?;
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                for (k, v) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, k, v as i32);
                }
                textures.push(t);
            }
            Ok(Self { gl, program, vao, textures: textures.try_into().unwrap(), nv12, picture: None })
        }
    }

    pub fn upload(&mut self, pic: &Picture) {
        let (w, h) = (pic.width as usize, pic.height as usize);
        let planes: Vec<(u32, i32, i32, u32)> = (0..pic.format.planes())
            .map(|i| {
                let (bytes, rows) = pic.format.plane_size(i, w, h);
                match (pic.format, i) {
                    (PixelFormat::Nv12, 1) => (glow::RG8, bytes as i32 / 2, rows as i32, glow::RG),
                    _ => (glow::R8, bytes as i32, rows as i32, glow::RED),
                }
            })
            .collect();
        let gl = &self.gl;
        // SAFETY: the context is current; each plane holds `stride * rows`
        // bytes, as the row length below says.
        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            for (i, &(internal, pw, ph, format)) in planes.iter().enumerate() {
                let bytes_per_px = if format == glow::RG { 2 } else { 1 };
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, (pic.strides[i] / bytes_per_px) as i32);
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[i]));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    internal as i32,
                    pw,
                    ph,
                    0,
                    format,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pic.planes[i])),
                );
            }
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(Some(&self.nv12), (pic.format == PixelFormat::Nv12) as i32);
        }
        self.picture = Some((pic.width, pic.height));
    }

    /// Draws the last uploaded picture; returns where it went.
    pub fn draw(&self, window: (u32, u32)) -> Option<Placement> {
        let gl = &self.gl;
        // SAFETY: the context is current.
        unsafe {
            gl.viewport(0, 0, window.0 as i32, window.1 as i32);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            let picture = self.picture?;
            let p = Placement::fit(window, picture);
            // GL's origin is bottom left.
            let y = window.1 as f64 - p.y - p.height;
            gl.viewport(p.x as i32, y as i32, p.width.round() as i32, p.height.round() as i32);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            for (i, t) in self.textures.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(*t));
            }
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            Some(p)
        }
    }

    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.picture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterboxes_and_maps_back() {
        let p = Placement::fit((2000, 1000), (1000, 1000));
        assert_eq!((p.x, p.y, p.width, p.height), (500.0, 0.0, 1000.0, 1000.0));
        assert_eq!(p.to_picture(750.0, 500.0), (250.0, 500.0));
        let p = Placement::fit((960, 540), (1920, 1080));
        assert_eq!(p.to_picture(480.0, 270.0), (960.0, 540.0));
    }
}
