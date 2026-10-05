//! The host as a Wayland client of the nested compositor, for its output
//! scale (docs/design.md §5). labwc ignores the `preferred_scale` the host
//! sends its window, so scale goes through `wlr-output-management`, as
//! `wlr-randr --scale` would do it. The nested compositor's own heads also
//! say when a new size and scale have taken effect, which ends the hold on
//! frames across a layout change (`pipeline`).
//!
//! The nested compositor's socket is found from its process: the listening
//! Unix socket it holds whose name looks like a Wayland display.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, bail};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1, zwlr_output_manager_v1,
    zwlr_output_mode_v1,
};

use crate::host::Host;

/// One of the nested compositor's outputs, as it reports it.
#[derive(Debug, Default)]
struct Head {
    proxy: Option<zwlr_output_head_v1::ZwlrOutputHeadV1>,
    name: String,
    enabled: bool,
    scale: f64,
    current_mode: Option<zwlr_output_mode_v1::ZwlrOutputModeV1>,
}

#[derive(Default)]
struct State {
    manager: Option<zwlr_output_manager_v1::ZwlrOutputManagerV1>,
    heads: Vec<Head>,
    /// Each mode's size.
    modes: Vec<(zwlr_output_mode_v1::ZwlrOutputModeV1, (i32, i32))>,
    /// The serial of the last complete description; 0 before the first.
    serial: u32,
    /// The scale to apply once there is a description to apply it to.
    want: Option<f64>,
    /// A configuration is on its way.
    applying: bool,
}

/// The connection to the nested compositor.
pub struct Outputs {
    /// Tells this connection from one to a restarted desktop.
    generation: u64,
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
}

/// The nested compositor's first enabled output: its size and scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputState {
    pub size: (i32, i32),
    pub scale: f64,
}

impl Outputs {
    /// Connects to the compositor running as `pid`, and dispatches its
    /// events on the host's event loop. Never waits for the compositor: it
    /// is our client too, and may be waiting for us.
    pub fn connect(host: &mut Host, pid: i32) -> anyhow::Result<()> {
        let path = wayland_socket(pid)?;
        let stream = UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
        let conn = Connection::from_socket(stream)?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        conn.flush()?;
        tracing::info!(socket = %path.display(), "connecting to the nested compositor");
        let state = State::default();
        let fd = conn.as_fd().try_clone_to_owned()?;
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        host.loop_handle
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), move |_, _, host| {
                if host.outputs.as_ref().is_none_or(|o| o.generation != generation) {
                    return Ok(PostAction::Remove); // a desktop that has gone
                }
                if let Some(outputs) = host.outputs.as_mut()
                    && let Err(err) = outputs.dispatch()
                {
                    tracing::warn!("the nested compositor's connection: {err:#}");
                    host.outputs = None;
                    return Ok(PostAction::Remove);
                }
                crate::pipeline::outputs_changed(host);
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        host.outputs = Some(Outputs { generation, conn, queue, state });
        Ok(())
    }

    fn dispatch(&mut self) -> anyhow::Result<()> {
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        self.apply();
        self.conn.flush()?;
        Ok(())
    }

    /// Sets every enabled output's scale.
    pub fn set_scale(&mut self, scale: f64) {
        self.state.want = Some(scale);
        self.apply();
        let _ = self.conn.flush();
    }

    fn apply(&mut self) {
        let s = &mut self.state;
        let (Some(scale), Some(manager)) = (s.want, s.manager.as_ref()) else { return };
        if s.serial == 0 || s.applying {
            return;
        }
        if s.heads.iter().filter(|h| h.enabled).all(|h| (h.scale - scale).abs() < 1e-3) {
            s.want = None;
            return;
        }
        let qh = self.queue.handle();
        let config = manager.create_configuration(s.serial, &qh, ());
        for head in &s.heads {
            let Some(proxy) = &head.proxy else { continue };
            if head.enabled {
                config.enable_head(proxy, &qh, ()).set_scale(scale);
            } else {
                config.disable_head(proxy);
            }
        }
        config.apply();
        s.applying = true;
        tracing::info!(scale, "setting the nested compositor's scale");
    }

    pub fn current(&self) -> Option<OutputState> {
        let head = self.state.heads.iter().find(|h| h.enabled)?;
        let mode = head.current_mode.as_ref()?;
        let size = self.state.modes.iter().find(|(m, _)| m == mode)?.1;
        Some(OutputState { size, scale: head.scale })
    }
}

/// The listening Wayland socket of process `pid`.
pub(crate) fn wayland_socket(pid: i32) -> anyhow::Result<PathBuf> {
    let mut inodes = Vec::new();
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))?.flatten() {
        if let Ok(target) = std::fs::read_link(fd.path())
            && let Some(inode) = target.to_str().and_then(|t| t.strip_prefix("socket:[")?.strip_suffix(']'))
        {
            inodes.push(inode.to_string());
        }
    }
    // Num RefCount Protocol Flags Type St Inode Path; listening sockets
    // have the accept flag (0x10000).
    for line in std::fs::read_to_string("/proc/net/unix")?.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let listening = u32::from_str_radix(f[3], 16).is_ok_and(|flags| flags & 0x10000 != 0);
        let path = PathBuf::from(f[7]);
        let wayland = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("wayland-") && !n.ends_with(".lock"));
        if listening && wayland && inodes.iter().any(|i| i == f[6]) {
            return Ok(path);
        }
    }
    bail!("process {pid} has no Wayland socket")
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e
            && interface == zwlr_output_manager_v1::ZwlrOutputManagerV1::interface().name
        {
            s.manager = Some(r.bind(name, version.min(4), qh, ()));
        }
    }
}

impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        e: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_manager_v1::Event::Head { head } => {
                s.heads.push(Head { proxy: Some(head), scale: 1.0, ..Default::default() });
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                if s.serial == 0 {
                    tracing::info!(heads = ?s.heads.iter().map(|h| &h.name).collect::<Vec<_>>(), "the nested compositor's outputs");
                }
                s.serial = serial;
            }
            _ => {}
        }
    }

    event_created_child!(State, zwlr_output_manager_v1::ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (zwlr_output_head_v1::ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for State {
    fn event(
        s: &mut Self,
        proxy: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        e: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(head) = s.heads.iter_mut().find(|h| h.proxy.as_ref() == Some(proxy)) else { return };
        match e {
            zwlr_output_head_v1::Event::Name { name } => head.name = name,
            zwlr_output_head_v1::Event::Enabled { enabled } => head.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Scale { scale } => head.scale = scale,
            zwlr_output_head_v1::Event::CurrentMode { mode } => head.current_mode = Some(mode),
            zwlr_output_head_v1::Event::Finished => {
                s.heads.retain(|h| h.proxy.as_ref() != Some(proxy));
            }
            _ => {}
        }
    }

    event_created_child!(State, zwlr_output_head_v1::ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (zwlr_output_mode_v1::ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<zwlr_output_mode_v1::ZwlrOutputModeV1, ()> for State {
    fn event(
        s: &mut Self,
        proxy: &zwlr_output_mode_v1::ZwlrOutputModeV1,
        e: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                match s.modes.iter_mut().find(|(m, _)| m == proxy) {
                    Some(m) => m.1 = (width, height),
                    None => s.modes.push((proxy.clone(), (width, height))),
                }
            }
            zwlr_output_mode_v1::Event::Finished => s.modes.retain(|(m, _)| m != proxy),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        s: &mut Self,
        proxy: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        e: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_configuration_v1::Event::Succeeded => {}
            // Outdated: tried again after the next `done`.
            zwlr_output_configuration_v1::Event::Cancelled => {}
            zwlr_output_configuration_v1::Event::Failed => {
                tracing::warn!(scale = ?s.want, "the nested compositor refused the scale");
                s.want = None;
            }
            _ => return,
        }
        s.applying = false;
        proxy.destroy();
    }
}

impl Dispatch<zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
