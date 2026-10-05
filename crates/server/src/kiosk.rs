//! Kiosk mode (docs/design.md §6): one app, no nested compositor. The app
//! connects to the host itself, which shows it fullscreen, composites its
//! surfaces and popups into the picture that gets encoded, and applies the
//! client's scale directly, in one step (§5).
//!
//! The host's logical coordinates are the output's pixels divided by its
//! scale here, as in any compositor; in nested mode they are the pixels
//! themselves. Clipboard and text input aren't offered to the app yet:
//! they come from the nested compositor in the usual mode.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Offscreen};
use smithay::desktop::{PopupKind, PopupManager, Window, WindowSurfaceType};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Scale, Size, Transform};

use farsight_proto::tiles::Rect;

use crate::host::Host;

struct Target {
    texture: GlesTexture,
    tracker: OutputDamageTracker,
    size: (i32, i32),
    scale: f64,
}

#[derive(Default)]
pub struct Kiosk {
    pub popups: PopupManager,
    /// The app's window, once it has one.
    pub window: Option<Window>,
    /// The composited picture, kept between frames, and what tracks what
    /// changed on it.
    target: Option<Target>,
}

impl Kiosk {
    /// The surface under `point` (logical, output coordinates) and its
    /// origin, popups first.
    pub fn surface_under(&self, point: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let window = self.window.as_ref()?;
        let origin = Point::<i32, Logical>::default() - window.geometry().loc;
        let (surface, loc) = window.surface_under(point - origin.to_f64(), WindowSurfaceType::ALL)?;
        Some((surface, (loc + origin).to_f64()))
    }

    /// Closes every open popup, as a click outside them does.
    pub fn dismiss_popups(&mut self) {
        let Some(surface) = self.window.as_ref().and_then(|w| w.toplevel()).map(|t| t.wl_surface().clone()) else {
            return;
        };
        let popups: Vec<PopupKind> = PopupManager::popups_for_surface(&surface).map(|(p, _)| p).collect();
        for popup in popups.iter().rev() {
            let _ = PopupManager::dismiss_popup(&surface, popup);
        }
    }

    /// The composited picture as it stands.
    pub fn texture(&self) -> Option<GlesTexture> {
        self.target.as_ref().map(|t| t.texture.clone())
    }

    /// Draws the app at `size` pixels and `scale`, and returns the picture
    /// with what changed on it.
    pub fn composite(
        &mut self,
        renderer: &mut GlesRenderer,
        size: (i32, i32),
        scale: f64,
    ) -> anyhow::Result<Option<(GlesTexture, Vec<Rect>)>> {
        let Some(window) = &self.window else { return Ok(None) };
        let psize: Size<i32, Physical> = size.into();
        if self.target.as_ref().is_none_or(|t| t.size != size || t.scale != scale) {
            let texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, (size.0, size.1).into())?;
            let tracker = OutputDamageTracker::new(psize, scale, Transform::Normal);
            self.target = Some(Target { texture, tracker, size, scale });
        }
        let Target { texture, tracker, .. } = self.target.as_mut().unwrap();
        // The window's geometry starts at the output's corner.
        let origin = (Point::<i32, Logical>::default() - window.geometry().loc).to_physical_precise_round(scale);
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, origin, Scale::from(scale), 1.0);
        let damage = {
            let mut fb = renderer.bind(texture)?;
            // One persistent buffer: what is on it is the last frame.
            let result = tracker
                .render_output(renderer, &mut fb, 1, &elements, [0.0, 0.0, 0.0, 1.0])
                .map_err(|e| anyhow::anyhow!("compositing the app: {e:?}"))?;
            result.damage.cloned().unwrap_or_default()
        };
        let rects = damage
            .iter()
            .map(|r| {
                let (x, y) = (r.loc.x.max(0), r.loc.y.max(0));
                Rect::new(x as u16, y as u16, (r.size.w - (x - r.loc.x)).max(0) as u16, (r.size.h - (y - r.loc.y)).max(0) as u16)
            })
            .collect();
        Ok(Some((texture.clone(), rects)))
    }
}

/// A commit to any of the app's surfaces.
pub fn commit(host: &mut Host, surface: &WlSurface) {
    let Some(kiosk) = host.kiosk.as_mut() else { return };
    kiosk.popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = kiosk.popups.find_popup(surface)
        && !popup.is_initial_configure_sent()
    {
        let _ = popup.send_configure();
    }
    if let Some(window) = &kiosk.window {
        window.on_commit();
    }
    crate::pipeline::on_kiosk_commit(host);
}

/// The app's size in output pixels, as it last drew itself.
pub fn drawn_size(host: &Host) -> Option<(i32, i32)> {
    let window = host.kiosk.as_ref()?.window.as_ref()?;
    let size = window.geometry().size;
    let scale = host.layout.scale;
    Some(((size.w as f64 * scale).round() as i32, (size.h as f64 * scale).round() as i32))
}
