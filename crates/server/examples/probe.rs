//! M0 spike probe: a Wayland app that runs inside the nested desktop and
//! reports what an app sees.
//!
//! - Paints its frame number over the top three quarters of the window
//!   (R = low byte, G = high byte, B = 0xA5) and logs each commit's CLOCK_MONOTONIC time, so the
//!   server (`--probe`) can measure app commit → encoded latency.
//! - Logs configure sizes, preferred buffer/fractional scale, the keymap it
//!   is given and each key with its keysym.
//!
//! `PROBE_INTERVAL_MS=N` paints every N ms (an app updating now and then)
//! instead of on every frame callback (an app animating).

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const POOL_BYTES: usize = 2 * 3840 * 2160 * 4;

fn mono_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

macro_rules! log {
    ($($t:tt)*) => {{
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "probe t={} {}", mono_us(), format!($($t)*));
        let _ = out.flush();
    }};
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    surface: Option<wl_surface::WlSurface>,
    viewport: Option<wp_viewport::WpViewport>,
    pool: Option<wl_shm_pool::WlShmPool>,
    mem: Option<(*mut u8, OwnedFd)>,
    buffers: Vec<(wl_buffer::WlBuffer, bool, (i32, i32))>,
    logical: (i32, i32),
    buffer_scale: i32,
    fractional_scale: Option<f64>,
    configured: bool,
    frame_pending: bool,
    seq: u32,
    xkb: Option<(xkbcommon::xkb::Keymap, xkbcommon::xkb::State)>,
    closed: bool,
}

impl App {
    fn physical(&self) -> (i32, i32) {
        let s = self.fractional_scale.unwrap_or(self.buffer_scale as f64);
        ((self.logical.0 as f64 * s).round() as i32, (self.logical.1 as f64 * s).round() as i32)
    }

    fn draw(&mut self, qh: &QueueHandle<App>) {
        if !self.configured {
            return;
        }
        let (w, h) = self.physical();
        let slot = (0..2).find(|&i| self.buffers.get(i).is_none_or(|b| !b.1 || b.2 != (w, h)));
        let Some(slot) = slot else { return }; // both busy
        let offset = slot * POOL_BYTES / 2;
        if self.buffers.get(slot).is_none_or(|b| b.2 != (w, h)) {
            let buffer = self.pool.as_ref().unwrap().create_buffer(
                offset as i32, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, slot,
            );
            if let Some(old) = self.buffers.get(slot) {
                old.0.destroy();
            }
            if slot < self.buffers.len() {
                self.buffers[slot] = (buffer, false, (w, h));
            } else {
                self.buffers.push((buffer, false, (w, h)));
            }
        }
        self.seq = self.seq.wrapping_add(1) & 0xffff;
        let (base, _) = self.mem.as_ref().unwrap();
        // SAFETY: the pool is POOL_BYTES long and each half fits a 4K frame.
        let px = unsafe {
            std::slice::from_raw_parts_mut(base.add(offset) as *mut u32, (w * h) as usize)
        };
        let block = 0xff00_00a5 | (self.seq & 0xff) << 16 | (self.seq >> 8 & 0xff) << 8;
        // Bands of 1-pixel lines show whether scaling is sharp.
        for y in 0..h {
            let row = &mut px[(y * w) as usize..((y + 1) * w) as usize];
            for (x, p) in row.iter_mut().enumerate() {
                let x = x as i32;
                *p = if y < h * 3 / 4 {
                    block
                } else if (x / 2) % 2 == 0 {
                    0xffffffff
                } else {
                    0xff202020
                };
            }
        }
        let surface = self.surface.as_ref().unwrap();
        let b = &mut self.buffers[slot];
        b.1 = true;
        surface.attach(Some(&b.0), 0, 0);
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.logical.0, self.logical.1);
        } else {
            surface.set_buffer_scale(self.buffer_scale);
        }
        surface.damage_buffer(0, 0, w, h);
        surface.frame(qh, ());
        self.frame_pending = true;
        surface.commit();
        log!("commit seq={} buffer={}x{}", self.seq, w, h);
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let mut queue: EventQueue<App> = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut app = App { buffer_scale: 1, ..Default::default() };
    queue.roundtrip(&mut app).unwrap();

    let name = c"probe";
    // SAFETY: memfd_create with a valid name.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    // SAFETY: fd is a fresh, owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: plain syscalls on our own fd.
    let base = unsafe {
        libc::ftruncate(fd.as_raw_fd(), POOL_BYTES as i64);
        libc::mmap(std::ptr::null_mut(), POOL_BYTES, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    } as *mut u8;
    app.pool = Some(app.shm.as_ref().unwrap().create_pool(fd.as_fd(), POOL_BYTES as i32, &qh, ()));
    app.mem = Some((base, fd));

    let surface = app.compositor.as_ref().unwrap().create_surface(&qh, ());
    if let Some(v) = &app.viewporter {
        app.viewport = Some(v.get_viewport(&surface, &qh, ()));
    }
    if let Some(f) = &app.fractional {
        f.get_fractional_scale(&surface, &qh, ());
    }
    let xdg = app.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("farsight probe".into());
    // PROBE_FULLSCREEN=0 keeps it windowed, so labwc composites it instead of
    // passing its buffer straight through.
    if std::env::var("PROBE_FULLSCREEN").as_deref() != Ok("0") {
        toplevel.set_fullscreen(None);
    }
    surface.commit();
    app.surface = Some(surface);

    let interval = std::env::var("PROBE_INTERVAL_MS").ok().and_then(|s| s.parse::<u64>().ok());
    log!("start interval_ms={interval:?}");
    let mut next_paint = mono_us();
    while !app.closed {
        queue.flush().unwrap();
        let timeout = match interval {
            Some(ms) => {
                let now = mono_us();
                if now >= next_paint && !app.frame_pending {
                    app.draw(&qh);
                    next_paint = now + ms * 1000;
                    continue;
                }
                Duration::from_micros(next_paint.saturating_sub(now).max(500))
            }
            None => Duration::from_millis(100),
        };
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
            if n > 0 {
                let _ = guard.read();
            }
        }
        queue.dispatch_pending(&mut app).unwrap();
        if interval.is_none() && !app.frame_pending && app.configured {
            app.draw(&qh);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(app: &mut Self, reg: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_compositor" => app.compositor = Some(reg.bind(name, version.min(6), qh, ())),
            "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
            "xdg_wm_base" => app.wm_base = Some(reg.bind(name, 1, qh, ())),
            "wp_viewporter" => app.viewporter = Some(reg.bind(name, 1, qh, ())),
            "wp_fractional_scale_manager_v1" => app.fractional = Some(reg.bind(name, 1, qh, ())),
            "wl_seat" => {
                reg.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(app: &mut Self, _: &wl_surface::WlSurface, event: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            log!("preferred_buffer_scale {factor}");
            app.buffer_scale = factor;
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for App {
    fn event(app: &mut Self, _: &wp_fractional_scale_v1::WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let s = scale as f64 / 120.0;
            log!("preferred_fractional_scale {s}");
            app.fractional_scale = Some(s);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(app: &mut Self, xdg: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            app.configured = true;
            log!("configure logical={}x{} physical={:?}", app.logical.0, app.logical.1, app.physical());
            if !app.frame_pending {
                app.draw(qh);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(app: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                app.logical = (width, height);
            }
            xdg_toplevel::Event::Configure { .. } if app.logical == (0, 0) => app.logical = (640, 480),
            xdg_toplevel::Event::Close => app.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(app: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            app.frame_pending = false;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for App {
    fn event(app: &mut Self, _: &wl_buffer::WlBuffer, event: wl_buffer::Event, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event
            && let Some(b) = app.buffers.get_mut(*slot) {
                b.1 = false;
            }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(_: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event
            && caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(app: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use xkbcommon::xkb;
        match event {
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                // SAFETY: the compositor sent a keymap of `size` bytes in fd.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(&ctx, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::COMPILE_NO_FLAGS)
                };
                if let Ok(Some(keymap)) = keymap {
                    let layouts: Vec<String> = (0..keymap.num_layouts()).map(|i| keymap.layout_get_name(i).to_string()).collect();
                    log!("keymap layouts={layouts:?}");
                    let state = xkb::State::new(&keymap);
                    app.xkb = Some((keymap, state));
                }
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                if let Some((_, xkb_state)) = &app.xkb {
                    let code = xkb::Keycode::new(key + 8);
                    let sym = xkb_state.key_get_one_sym(code);
                    let utf8 = xkb_state.key_get_utf8(code);
                    log!("key evdev={key} state={state:?} keysym={} utf8={utf8:?}", xkb::keysym_get_name(sym));
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some((_, s)) = &mut app.xkb {
                    s.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Enter { .. } => log!("keyboard enter"),
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wp_viewporter::WpViewporter);
delegate_noop!(App: ignore wp_viewport::WpViewport);
delegate_noop!(App: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
