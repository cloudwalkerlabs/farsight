//! The session's clipboard (docs/design.md §8), through `ext-data-control`
//! as a client of the nested compositor, like `outputs`.
//!
//! - The session's selection changes: its MIME types go to the controlling
//!   client, which fetches the data only when it needs it
//!   ([`Clipboard::read`]).
//! - The client's clipboard changes: the host sets a selection of its own
//!   with the client's MIME types, and fetches from the client when an app
//!   pastes ([`Clipboard::take_sends`]).
//!
//! Data moves through pipes, read and written off the event loop.

use std::io::Read;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::Context;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1, ext_data_control_source_v1,
};

use farsight_proto::control::{ClipboardOffer, MAX_CLIPBOARD};

use crate::host::Host;

type Offer = ext_data_control_offer_v1::ExtDataControlOfferV1;
type Source = ext_data_control_source_v1::ExtDataControlSourceV1;

/// A paste in the session waiting for the client's data.
#[derive(Debug)]
pub struct Send {
    /// The client's offer it wants.
    pub serial: u32,
    pub mime: String,
    /// Where the data goes; closing it ends the paste.
    pub fd: OwnedFd,
}

#[derive(Default)]
struct State {
    manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<ext_data_control_device_v1::ExtDataControlDeviceV1>,
    /// Offers being described, with their MIME types so far.
    offers: Vec<(Offer, Vec<String>)>,
    /// The session's selection, unless it is ours.
    selection: Option<(Offer, Vec<String>)>,
    /// Our selection, standing for the client's clipboard, and its serial.
    source: Option<(Source, u32)>,
    /// The next selection event is ours coming back.
    expect_own: bool,
    /// The session's selection changed and the client hasn't heard.
    changed: bool,
    sends: Vec<Send>,
}

pub struct Clipboard {
    generation: u64,
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// Counts the session's selections.
    serial: u32,
}

impl Clipboard {
    /// Connects to the compositor running as `pid`; its events are handled
    /// on the host's event loop.
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
                let Some(c) = host.clipboard.as_mut().filter(|c| c.generation == generation) else {
                    return Ok(PostAction::Remove); // a desktop that has gone
                };
                if let Err(err) = c.dispatch() {
                    tracing::warn!("the nested compositor's clipboard: {err:#}");
                    host.clipboard = None;
                    return Ok(PostAction::Remove);
                }
                changed(host);
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        host.clipboard = Some(Clipboard { generation, conn, queue, state: State::default(), serial: 0 });
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
        if s.device.is_none()
            && let (Some(manager), Some(seat)) = (&s.manager, &s.seat)
        {
            s.device = Some(manager.get_data_device(seat, &self.queue.handle(), ()));
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The session's selection, if it changed since the last call.
    pub fn take_change(&mut self) -> Option<ClipboardOffer> {
        if !std::mem::take(&mut self.state.changed) {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        Some(self.offer())
    }

    /// The session's selection as it is.
    pub fn offer(&self) -> ClipboardOffer {
        let mimes = self.state.selection.as_ref().map(|(_, m)| m.clone()).unwrap_or_default();
        ClipboardOffer { serial: self.serial, mimes }
    }

    /// Pastes waiting for the client's data.
    pub fn take_sends(&mut self) -> Vec<Send> {
        std::mem::take(&mut self.state.sends)
    }

    /// Reads selection `serial` as `mime`, off the event loop. `None` if
    /// the selection has changed, or the app holding it gives nothing.
    pub fn read(&mut self, serial: u32, mime: &str, reply: impl FnOnce(Option<Vec<u8>>) + std::marker::Send + 'static) {
        let Some((offer, mimes)) = self.state.selection.as_ref().filter(|_| serial == self.serial) else {
            return reply(None);
        };
        if !mimes.iter().any(|m| m == mime) {
            return reply(None);
        }
        let Ok((read, write)) = pipe() else { return reply(None) };
        offer.receive(mime.to_string(), write.as_fd());
        let _ = self.conn.flush();
        drop(write);
        std::thread::spawn(move || reply(read_all(read)));
    }

    /// The client's clipboard changed: offer its types in the session.
    pub fn set_client_offer(&mut self, offer: &ClipboardOffer) {
        let Some(manager) = &self.state.manager else { return };
        let Some(device) = &self.state.device else { return };
        if let Some((old, _)) = self.state.source.take() {
            old.destroy();
        }
        if offer.mimes.is_empty() {
            device.set_selection(None);
        } else {
            let source = manager.create_data_source(&self.queue.handle(), ());
            for m in &offer.mimes {
                source.offer(m.clone());
            }
            device.set_selection(Some(&source));
            self.state.source = Some((source, offer.serial));
        }
        self.state.expect_own = true;
        let _ = self.conn.flush();
    }
}

/// Tells the controlling client the session's selection changed, and hands
/// pastes of the client's clipboard to the network.
fn changed(host: &mut Host) {
    let Some(c) = host.clipboard.as_mut() else { return };
    let change = c.take_change();
    let sends = c.take_sends();
    let Some(client) = host.client else { return };
    if let Some(offer) = change {
        tracing::debug!(mimes = ?offer.mimes, "the session's clipboard changed");
        let msg = farsight_proto::control::ServerMessage::ClipboardOffer(offer);
        let _ = host.net.send(crate::net::ToNet::Message(client, msg));
    }
    for send in sends {
        let _ = host.net.send(crate::net::ToNet::FetchClipboard(client, send));
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both ends.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both are new descriptors we own.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Everything from `fd` until the writer closes it, up to the limit and
/// for at most five seconds.
fn read_all(fd: OwnedFd) -> Option<Vec<u8>> {
    let mut file = std::fs::File::from(fd);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    let mut buf = vec![0; 64 << 10];
    loop {
        if Instant::now() > deadline || out.len() > MAX_CLIPBOARD {
            tracing::warn!(bytes = out.len(), "clipboard data too slow or too large; dropped");
            return None;
        }
        match file.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            if interface == ext_data_control_manager_v1::ExtDataControlManagerV1::interface().name {
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

impl Dispatch<ext_data_control_manager_v1::ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ext_data_control_manager_v1::ExtDataControlManagerV1,
        _: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ext_data_control_device_v1::ExtDataControlDeviceV1,
        e: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_data_control_device_v1::Event::DataOffer { id } => s.offers.push((id, Vec::new())),
            ext_data_control_device_v1::Event::Selection { id } => {
                let new = id.and_then(|id| {
                    let i = s.offers.iter().position(|(o, _)| *o == id)?;
                    Some(s.offers.swap_remove(i))
                });
                if let Some((old, _)) = s.selection.take() {
                    old.destroy();
                }
                if std::mem::take(&mut s.expect_own) {
                    // Our own selection, standing for the client's: nothing
                    // to tell the client.
                    if let Some((o, _)) = new {
                        o.destroy();
                    }
                    return;
                }
                s.selection = new;
                s.changed = true;
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(id) } => {
                if let Some(i) = s.offers.iter().position(|(o, _)| *o == id) {
                    s.offers.swap_remove(i).0.destroy();
                }
            }
            ext_data_control_device_v1::Event::Finished => s.device = None,
            _ => {}
        }
    }

    event_created_child!(State, ext_data_control_device_v1::ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (Offer, ()),
    ]);
}

impl Dispatch<Offer, ()> for State {
    fn event(s: &mut Self, proxy: &Offer, e: ext_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = e
            && let Some((_, mimes)) = s.offers.iter_mut().find(|(o, _)| o == proxy)
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<Source, ()> for State {
    fn event(s: &mut Self, proxy: &Source, e: ext_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Some((_, serial)) = s.source.as_ref().filter(|(src, _)| src == proxy) {
                    s.sends.push(Send { serial: *serial, mime: mime_type, fd });
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                if s.source.as_ref().is_some_and(|(src, _)| src == proxy) {
                    s.source = None;
                }
                proxy.destroy();
            }
            _ => {}
        }
    }
}
