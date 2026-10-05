//! Screenshots and virtual input for a headless wlroots compositor, to
//! drive the desktop client in tests (tools/m1/e2e.sh). Talks to
//! `$WAYLAND_DISPLAY`.
//!
//!   wltool shot OUT.ppm      the first output; OVERLAY=1 includes the cursor
//!   wltool move X Y          absolute pointer position, in output pixels
//!   wltool click [BUTTON]    evdev button code, BTN_LEFT by default
//!   wltool wheel N           N wheel clicks, positive is down
//!   wltool key CODE...       evdev key codes, each pressed and released;
//!                            +CODE holds one until the end (for Shift)
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

#[derive(Default)]
struct S {
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    seat: Option<wl_seat::WlSeat>,
    copy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    vptr: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    vkbd: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    size: (i32, i32),
    fmt: Option<(wl_shm::Format, u32, u32, u32)>,
    done: bool,
    failed: bool,
    flags: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for S {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            match interface.as_str() {
                "wl_shm" => s.shm = Some(r.bind(name, 1, qh, ())),
                "wl_output" if s.output.is_none() => s.output = Some(r.bind(name, 1, qh, ())),
                "wl_seat" => s.seat = Some(r.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => s.copy = Some(r.bind(name, version.min(3), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => s.vptr = Some(r.bind(name, 1, qh, ())),
                "zwp_virtual_keyboard_manager_v1" => s.vkbd = Some(r.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for S {
    fn event(s: &mut Self, _: &wl_output::WlOutput, e: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Mode { width, height, flags: WEnum::Value(f), .. } = e
            && f.contains(wl_output::Mode::Current)
        {
            s.size = (width, height);
        }
    }
}
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for S {
    fn event(s: &mut Self, _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, e: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event::*;
        match e {
            Buffer { format: WEnum::Value(f), width, height, stride } => {
                if s.fmt.is_none() {
                    s.fmt = Some((f, width, height, stride))
                }
            }
            Flags { flags: WEnum::Value(f) } => s.flags = f.bits(),
            Ready { .. } => s.done = true,
            Failed => s.failed = true,
            _ => {}
        }
    }
}
delegate_noop!(S: ignore wl_shm::WlShm);
delegate_noop!(S: ignore wl_seat::WlSeat);
delegate_noop!(S: ignore wl_shm_pool::WlShmPool);
delegate_noop!(S: ignore wl_buffer::WlBuffer);
delegate_noop!(S: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(S: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(S: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(S: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(S: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

fn memfd(size: usize) -> std::fs::File {
    // SAFETY: plain syscalls.
    unsafe {
        let fd = libc::memfd_create(c"wltool".as_ptr(), 0);
        assert!(fd >= 0);
        libc::ftruncate(fd, size as i64);
        std::fs::File::from(OwnedFd::from_raw_fd(fd))
    }
}

fn ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000) as u32
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let conn = Connection::connect_to_env().unwrap();
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut s = S::default();
    q.roundtrip(&mut s).unwrap();
    q.roundtrip(&mut s).unwrap();
    let num = |i: usize| args[i].parse::<f64>().unwrap();
    match args[0].as_str() {
        "shot" => {
            let frame = s.copy.as_ref().unwrap().capture_output(std::env::var("OVERLAY").map_or(0, |_| 1), s.output.as_ref().unwrap(), &qh, ());
            while s.fmt.is_none() {
                q.blocking_dispatch(&mut s).unwrap();
            }
            let (f, w, h, stride) = s.fmt.unwrap();
            let size = (stride * h) as usize;
            let file = memfd(size);
            let pool = s.shm.as_ref().unwrap().create_pool(file.as_fd(), size as i32, &qh, ());
            let buf = pool.create_buffer(0, w as i32, h as i32, stride as i32, f, &qh, ());
            frame.copy(&buf);
            while !s.done && !s.failed {
                q.blocking_dispatch(&mut s).unwrap();
            }
            assert!(!s.failed, "screencopy failed");
            let data = std::fs::read(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&file))).unwrap();
            let mut out = std::fs::File::create(&args[1]).unwrap();
            write!(out, "P6\n{w} {h}\n255\n").unwrap();
            let yinvert = s.flags & 1 != 0;
            // Byte offsets of R, G and B in memory, and bytes per pixel.
            let (r, g, b, bpp) = match f {
                wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2, 1, 0, 4),
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => (0, 1, 2, 4),
                wl_shm::Format::Bgr888 => (0, 1, 2, 3),
                wl_shm::Format::Rgb888 => (2, 1, 0, 3),
                other => panic!("unsupported screencopy format {other:?}"),
            };
            for row in 0..h {
                let src = if yinvert { h - 1 - row } else { row };
                for px in data[(src * stride) as usize..][..(w * bpp) as usize].chunks(bpp as usize) {
                    out.write_all(&[px[r], px[g], px[b]]).unwrap();
                }
            }
            eprintln!("{w}x{h} {f:?}");
        }
        "move" | "click" | "wheel" => {
            let p = s.vptr.as_ref().unwrap().create_virtual_pointer(s.seat.as_ref(), &qh, ());
            match args[0].as_str() {
                "move" => {
                    let (w, h) = s.size;
                    p.motion_absolute(ms(), num(1) as u32, num(2) as u32, w as u32, h as u32);
                    p.frame();
                }
                "click" => {
                    let btn = args.get(1).map(|_| num(1) as u32).unwrap_or(0x110);
                    for st in [1, 0] {
                        p.button(ms(), btn, if st == 1 { wayland_client::protocol::wl_pointer::ButtonState::Pressed } else { wayland_client::protocol::wl_pointer::ButtonState::Released });
                        p.frame();
                    }
                }
                _ => {
                    let n = num(1);
                    p.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);
                    p.axis_discrete(ms(), wayland_client::protocol::wl_pointer::Axis::VerticalScroll, n * 15.0, n as i32);
                    p.frame();
                }
            }
            q.roundtrip(&mut s).unwrap();
            p.destroy();
            q.roundtrip(&mut s).unwrap();
        }
        "key" => {
            let k = s.vkbd.as_ref().unwrap().create_virtual_keyboard(s.seat.as_ref().unwrap(), &qh, ());
            let ctx = xkbcommon::xkb::Context::new(0);
            let km = xkbcommon::xkb::Keymap::new_from_names(&ctx, "", "", "us", "", None, 0).unwrap();
            let text = km.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);
            let mut file = memfd(0);
            file.write_all(text.as_bytes()).unwrap();
            file.write_all(&[0]).unwrap();
            k.keymap(1, file.as_fd(), text.len() as u32 + 1);
            q.roundtrip(&mut s).unwrap();
            // Clients drop keys that arrive while they load a new keymap.
            std::thread::sleep(std::time::Duration::from_millis(200));
            // "+" before a code holds it until the end (for shift).
            let mut held = Vec::new();
            for a in &args[1..] {
                if let Some(c) = a.strip_prefix('+') {
                    let c: u32 = c.parse().unwrap();
                    k.key(ms(), c, 1);
                    held.push(c);
                    continue;
                }
                let c: u32 = a.parse().unwrap();
                k.key(ms(), c, 1);
                q.roundtrip(&mut s).unwrap();
                k.key(ms(), c, 0);
                q.roundtrip(&mut s).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            for c in held {
                k.key(ms(), c, 0);
            }
            q.roundtrip(&mut s).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        _ => panic!("?"),
    }
}
