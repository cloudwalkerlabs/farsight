//! Text from the client's keyboard or IME (docs/design.md §4, §7): the host
//! is the session's input method, through `input-method-v2` as a client of
//! the nested compositor, like `outputs`. The nested compositor passes
//! commits on to the focused app over `text-input-v3`.
//!
//! Text only reaches apps that take `text-input-v3` and have a text field
//! focused; the client hears when that changes, to show its soft keyboard.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use anyhow::Context;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{zwp_input_method_manager_v2, zwp_input_method_v2};

use crate::host::Host;

#[derive(Default)]
struct State {
    manager: Option<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    seat: Option<wl_seat::WlSeat>,
    method: Option<zwp_input_method_v2::ZwpInputMethodV2>,
    /// As of the last `done`, and as pending before it.
    active: bool,
    pending: bool,
    /// `done` events so far: the serial for `commit`.
    done: u32,
    /// Another input method holds the seat.
    unavailable: bool,
}

pub struct Ime {
    generation: u64,
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// What the client last heard.
    told: Option<bool>,
}

impl Ime {
    pub fn connect(host: &mut Host, pid: i32) -> anyhow::Result<()> {
        let path = crate::outputs::wayland_socket(pid)?;
        let stream = UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
        let conn = Connection::from_socket(stream)?;
        let queue = conn.new_event_queue();
        conn.display().get_registry(&queue.handle(), ());
        conn.flush()?;
        let fd = conn.as_fd().try_clone_to_owned()?;
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        host.loop_handle
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), move |_, _, host| {
                let Some(ime) = host.ime.as_mut().filter(|i| i.generation == generation) else {
                    return Ok(PostAction::Remove);
                };
                if let Err(err) = ime.dispatch() {
                    tracing::warn!("the nested compositor's input method: {err:#}");
                    host.ime = None;
                    return Ok(PostAction::Remove);
                }
                text_input_changed(host);
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        host.ime = Some(Ime { generation, conn, queue, state: State::default(), told: None });
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
        let s = &mut self.state;
        if s.method.is_none()
            && !s.unavailable
            && let (Some(manager), Some(seat)) = (&s.manager, &s.seat)
        {
            s.method = Some(manager.get_input_method(seat, &self.queue.handle(), ()));
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Whether a text field takes text now.
    pub fn active(&self) -> bool {
        self.state.method.is_some() && self.state.active
    }

    /// Commits `text` to the focused field. False if no field takes it.
    pub fn commit(&mut self, text: &str) -> bool {
        let Some(m) = self.state.method.as_ref().filter(|_| self.state.active) else { return false };
        m.commit_string(text.to_string());
        m.commit(self.state.done);
        let _ = self.conn.flush();
        true
    }

    /// Shows `text` as being composed; empty clears it.
    pub fn preedit(&mut self, text: &str, cursor: Option<(u32, u32)>) {
        let Some(m) = self.state.method.as_ref().filter(|_| self.state.active) else { return };
        let (begin, end) = cursor.map_or((-1, -1), |(b, e)| (b as i32, e as i32));
        m.set_preedit_string(text.to_string(), begin, end);
        m.commit(self.state.done);
        let _ = self.conn.flush();
    }
}

/// Tells the controlling client when a text field gains or loses focus.
pub fn text_input_changed(host: &mut Host) {
    let Some(ime) = host.ime.as_mut() else { return };
    let active = ime.active();
    if ime.told == Some(active) && host.client.is_some() {
        return;
    }
    let Some(client) = host.client else {
        ime.told = None;
        return;
    };
    ime.told = Some(active);
    tracing::debug!(active, "text input");
    let msg = farsight_proto::control::ServerMessage::TextInput(active);
    let _ = host.net.send(crate::net::ToNet::Message(client, msg));
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            if interface == zwp_input_method_manager_v2::ZwpInputMethodManagerV2::interface().name {
                s.manager = Some(r.bind(name, version.min(1), qh, ()));
            } else if interface == wl_seat::WlSeat::interface().name && s.seat.is_none() {
                s.seat = Some(r.bind(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwp_input_method_manager_v2::ZwpInputMethodManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        _: zwp_input_method_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        e: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwp_input_method_v2::Event::Activate => s.pending = true,
            zwp_input_method_v2::Event::Deactivate => s.pending = false,
            zwp_input_method_v2::Event::Done => {
                s.done = s.done.wrapping_add(1);
                s.active = s.pending;
            }
            zwp_input_method_v2::Event::Unavailable => {
                tracing::warn!("another input method holds the nested compositor's seat; text input is off");
                s.unavailable = true;
                s.active = false;
                if let Some(m) = s.method.take() {
                    m.destroy();
                }
            }
            _ => {}
        }
    }
}
