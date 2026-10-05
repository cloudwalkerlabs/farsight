//! The cursor is drawn on the client (§4). The nested compositor sets its
//! cursor on the host's seat, as a named shape or as a surface; the host
//! reads the surface's pixels and sends the image once per connection
//! (cached by hash), then names it in `Cursor` messages.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::backend::renderer::{ExportMem, Texture};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Rectangle;
use smithay::wayland::compositor::with_states;

use farsight_proto::control::{CursorImage, CursorShape, ServerMessage};

use crate::host::Host;
use crate::net::ToNet;

#[derive(Default)]
pub struct Cursor {
    /// The nested compositor's cursor surface, while it uses one.
    surface: Option<WlSurface>,
    /// The cursor as it is now, and the image it names, if any.
    shape: Option<CursorShape>,
    image: Option<CursorImage>,
    /// Images the current client has.
    sent: HashSet<u64>,
}

impl Cursor {
    pub fn is_surface(&self, surface: &WlSurface) -> bool {
        self.surface.as_ref() == Some(surface)
    }
}

pub fn status_changed(host: &mut Host, status: CursorImageStatus) {
    match status {
        CursorImageStatus::Hidden => {
            host.cursor.surface = None;
            set_shape(host, CursorShape::Hidden);
        }
        CursorImageStatus::Named(icon) => {
            host.cursor.surface = None;
            set_shape(host, CursorShape::Named(icon.name().into()));
        }
        CursorImageStatus::Surface(surface) => {
            host.cursor.surface = Some(surface.clone());
            surface_committed(host, &surface);
        }
    }
}

pub fn surface_committed(host: &mut Host, surface: &WlSurface) {
    match read_image(host, surface) {
        Some(image) => {
            let id = image.id;
            host.cursor.image = Some(image);
            set_shape(host, CursorShape::Image(id));
        }
        // No buffer attached: nothing to draw.
        None => set_shape(host, CursorShape::Hidden),
    }
    let output = host.output.clone();
    smithay::desktop::utils::send_frames_surface_tree(
        surface,
        &output,
        Duration::from_micros(host.now_us()),
        None,
        |_, _| Some(output.clone()),
    );
}

/// A new client has no images yet; send it the cursor as it is.
pub fn client_connected(host: &mut Host) {
    host.cursor.sent.clear();
    if let Some(shape) = host.cursor.shape.clone() {
        send_shape(host, shape);
    }
}

fn set_shape(host: &mut Host, shape: CursorShape) {
    if host.cursor.shape.as_ref() != Some(&shape) {
        host.cursor.shape = Some(shape.clone());
        send_shape(host, shape);
    }
}

fn send_shape(host: &mut Host, shape: CursorShape) {
    let Some(client) = host.client else { return };
    if let CursorShape::Image(id) = shape
        && host.cursor.sent.insert(id)
        && let Some(image) = host.cursor.image.clone().filter(|i| i.id == id)
    {
        let _ = host.net.send(ToNet::Message(client, ServerMessage::CursorImage(image)));
    }
    let _ = host.net.send(ToNet::Message(client, ServerMessage::Cursor(shape)));
}

/// The cursor surface's pixels, as premultiplied BGRA with the hotspot in
/// buffer pixels.
fn read_image(host: &mut Host, surface: &WlSurface) -> Option<CursorImage> {
    let texture = crate::pipeline::nested_texture(host, surface)?;
    let (scale, logical_w) = with_renderer_surface_state(surface, |rs| {
        (rs.buffer_scale(), rs.surface_size().map(|s| s.w).unwrap_or(0))
    })
    .unwrap_or((1, 0));
    let hotspot = with_states(surface, |states| {
        states.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().unwrap().hotspot).unwrap_or_default()
    });
    let pixels = match read_pixels(host, &texture) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("reading the cursor image: {err:#}");
            return None;
        }
    };
    let size = texture.size();
    let (width, height) = (size.w as u32, size.h as u32);
    // The host's logical pixels are output pixels (§5): the image's
    // density is its width over its logical width, which covers both
    // buffer_scale and a viewport. The hotspot is logical.
    let scale_120 = match logical_w {
        w if w > 0 => width * farsight_proto::layout::SCALE_DENOMINATOR / w as u32,
        _ => scale as u32 * farsight_proto::layout::SCALE_DENOMINATOR,
    };
    let to_image = |v: i32| v * scale_120 as i32 / farsight_proto::layout::SCALE_DENOMINATOR as i32;
    let hotspot = (to_image(hotspot.x), to_image(hotspot.y));
    let mut hasher = DefaultHasher::new();
    (width, height, hotspot, scale_120, &pixels).hash(&mut hasher);
    Some(CursorImage { id: hasher.finish(), width, height, hotspot, scale_120, pixels })
}

fn read_pixels(host: &mut Host, texture: &GlesTexture) -> anyhow::Result<Vec<u8>> {
    let size = texture.size();
    let mapping = host.renderer.copy_texture(texture, Rectangle::from_size(size), Fourcc::Abgr8888)?;
    let rgba = host.renderer.map_texture(&mapping)?;
    // R, G, B, A in memory to B, G, R, A.
    let mut out = rgba.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(out)
}
