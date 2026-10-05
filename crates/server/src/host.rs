//! The host compositor: a headless Wayland compositor whose one real client
//! is the nested desktop compositor. It shows that client's toplevel as the
//! single fullscreen surface that gets encoded (docs/design.md, "Decision").

use std::time::Instant;

use smithay::backend::allocator::Buffer as _;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    self, CompositorClientState, CompositorHandler, CompositorState, with_states,
};
use smithay::wayland::dmabuf::{
    DmabufFeedback, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_dmabuf, delegate_fractional_scale, delegate_output,
    delegate_presentation, delegate_seat, delegate_shm, delegate_viewporter,
    delegate_xdg_decoration, delegate_xdg_shell,
};

use crate::cursor::Cursor;
use crate::net::{ConnId, ToNet};
use crate::pipeline::Pipeline;

/// Per-client data. The host has very few clients: the nested compositor,
/// and, in kiosk mode, one app.
#[derive(Default)]
pub struct ClientState {
    pub compositor: CompositorClientState,
}

impl smithay::reexports::wayland_server::backend::ClientData for ClientState {}

/// The output layout the client asked for (docs/design.md §5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    /// The client's refresh rate, which frame callbacks are paced to.
    pub refresh_mhz: u32,
}

pub struct Host {
    pub display: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Host>,
    pub start: Instant,
    pub compositor: CompositorState,
    pub xdg_shell: XdgShellState,
    pub shm: ShmState,
    pub dmabuf: DmabufState,
    /// Kept alive for the life of the host.
    pub _dmabuf_global: DmabufGlobal,
    pub seat_state: SeatState<Host>,
    pub seat: Seat<Host>,
    pub output: Output,
    pub layout: Layout,
    /// The nested compositor's window: the one surface we encode.
    pub toplevel: Option<ToplevelSurface>,
    pub renderer: GlesRenderer,
    pub pipeline: Pipeline,
    pub running: bool,
    /// The connected client, if any.
    pub client: Option<ConnId>,
    pub input: farsight_proto::input::InputReceiver,
    pub cursor: Cursor,
    pub net: tokio::sync::mpsc::UnboundedSender<ToNet>,
    /// labwc drops the configure that arrives before its output is enabled;
    /// we repeat it once after the first frame (see `pipeline`).
    pub initial_configure_repeated: bool,
    _globals: Vec<Box<dyn std::any::Any>>,
}

impl Host {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display: DisplayHandle,
        loop_handle: LoopHandle<'static, Host>,
        start: Instant,
        renderer: GlesRenderer,
        feedback: DmabufFeedback,
        pipeline: Pipeline,
        net: tokio::sync::mpsc::UnboundedSender<ToNet>,
        layout: Layout,
    ) -> Self {
        let dh = &display;
        let compositor = CompositorState::new_v6::<Self>(dh);
        let xdg_shell = XdgShellState::new::<Self>(dh);
        let shm = ShmState::new::<Self>(dh, vec![]);
        let mut dmabuf = DmabufState::new();
        let dmabuf_global =
            dmabuf.create_global_with_default_feedback::<Self>(dh, &feedback);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "seat0");
        seat.add_keyboard(Default::default(), 400, 30)
            .expect("default keymap");
        seat.add_pointer();
        seat.add_touch();

        let output = Output::new(
            "FARSIGHT-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "farsight".into(),
                model: "virtual".into(),
            },
        );
        output.create_global::<Self>(dh);

        let globals: Vec<Box<dyn std::any::Any>> = vec![
            Box::new(OutputManagerState::new_with_xdg_output::<Self>(dh)),
            Box::new(ViewporterState::new::<Self>(dh)),
            Box::new(FractionalScaleManagerState::new::<Self>(dh)),
            Box::new(PresentationState::new::<Self>(dh, libc::CLOCK_MONOTONIC as u32)),
            Box::new(XdgDecorationState::new::<Self>(dh)),
        ];

        let mut host = Self {
            display,
            loop_handle,
            start,
            compositor,
            xdg_shell,
            shm,
            dmabuf,
            _dmabuf_global: dmabuf_global,
            seat_state,
            seat,
            output,
            layout,
            toplevel: None,
            renderer,
            pipeline,
            running: true,
            client: None,
            input: Default::default(),
            cursor: Cursor::default(),
            net,
            initial_configure_repeated: false,
            _globals: globals,
        };
        host.apply_output(layout);
        host
    }

    /// Set the output mode and scale, and reconfigure the nested window.
    pub fn apply_output(&mut self, layout: Layout) {
        self.layout = layout;
        let mode = Mode { size: (layout.width, layout.height).into(), refresh: layout.refresh_mhz as i32 };
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(layout.scale)),
            Some((0, 0).into()),
        );
        self.output.set_preferred(mode);
        if let Some(toplevel) = &self.toplevel {
            configure_fullscreen(toplevel, &self.output, layout);
            toplevel.send_configure();
            let scale = layout.scale;
            with_states(toplevel.wl_surface(), |states| {
                compositor::send_surface_state(toplevel.wl_surface(), states, scale.ceil() as i32, Transform::Normal);
                smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                    fs.set_preferred_scale(scale);
                });
            });
        }
    }

    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// The nested window always fills the output, in physical pixels. The host's
/// own logical coordinate space is 1:1 with pixels; scale is only a hint to
/// the nested compositor (§5).
fn configure_fullscreen(toplevel: &ToplevelSurface, output: &Output, layout: Layout) {
    let wl_output = output.client_outputs(&toplevel.wl_surface().client().unwrap()).next();
    toplevel.with_pending_state(|state| {
        state.size = Some((layout.width, layout.height).into());
        state.states.set(xdg_toplevel::State::Fullscreen);
        state.states.set(xdg_toplevel::State::Activated);
        state.fullscreen_output = wl_output;
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
}

impl BufferHandler for Host {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for Host {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if self.cursor.is_surface(surface) {
            crate::cursor::surface_committed(self, surface);
            return;
        }
        let Some(toplevel) = self.toplevel.clone() else { return };
        if toplevel.wl_surface() != surface {
            if compositor::get_parent(surface).is_some() {
                tracing::warn!("subsurface commit; subsurfaces are not composited in the spike");
            }
            return;
        }
        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
            return;
        }
        crate::pipeline::on_toplevel_commit(self, &toplevel);
    }
}

impl ShmHandler for Host {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl DmabufHandler for Host {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let format = dmabuf.format();
        match self.renderer.import_dmabuf(&dmabuf, None) {
            Ok(_) => {
                tracing::info!(
                    fourcc = %format.code, modifier = ?format.modifier,
                    planes = dmabuf.num_planes(), size = ?dmabuf.size(),
                    "nested buffer imported"
                );
                let _ = notifier.successful::<Self>();
            }
            Err(err) => {
                tracing::warn!(fourcc = %format.code, modifier = ?format.modifier, %err, "dmabuf import failed");
                notifier.failed();
            }
        }
    }
}

impl XdgShellHandler for Host {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        if self.toplevel.is_some() {
            tracing::warn!("second toplevel; only one nested window is shown");
            return;
        }
        tracing::info!("nested window created");
        configure_fullscreen(&surface, &self.output, self.layout);
        self.output.enter(surface.wl_surface());
        let scale = self.layout.scale;
        with_states(surface.wl_surface(), |states| {
            compositor::send_surface_state(surface.wl_surface(), states, scale.ceil() as i32, Transform::Normal);
        });
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(kbd) = self.seat.get_keyboard() {
            kbd.set_focus(self, Some(surface.wl_surface().clone()), serial);
        }
        self.toplevel = Some(surface);
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        tracing::warn!("popup on the host; ignored");
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}

    fn fullscreen_request(&mut self, _surface: ToplevelSurface, _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.toplevel.as_ref() == Some(&surface) {
            tracing::info!("nested window destroyed");
            self.toplevel = None;
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let title = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().title.clone())
        });
        tracing::info!(?title, "nested window title");
    }
}

impl XdgDecorationHandler for Host {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}

impl SeatHandler for Host {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        crate::cursor::status_changed(self, image);
    }
}

impl OutputHandler for Host {}
impl FractionalScaleHandler for Host {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        tracing::info!("nested window bound wp_fractional_scale");
        let scale = self.layout.scale;
        with_states(&surface, |states| {
            smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale);
            });
        });
    }
}

delegate_compositor!(Host);
delegate_shm!(Host);
delegate_dmabuf!(Host);
delegate_xdg_shell!(Host);
delegate_xdg_decoration!(Host);
delegate_seat!(Host);
delegate_output!(Host);
delegate_viewporter!(Host);
delegate_fractional_scale!(Host);
delegate_presentation!(Host);
