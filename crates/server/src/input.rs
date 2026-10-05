//! Client input into the host's seat (§4). The nested compositor receives
//! it as it would input from real devices, and routes it to its apps.
//!
//! `farsight_proto::input::InputReceiver` has already turned the packets
//! into events applied exactly once, with corrections from snapshots.

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use farsight_proto::input::{InputEvent, InputPacket};

use crate::host::Host;

pub fn receive(host: &mut Host, packet: &InputPacket) {
    for event in host.input.receive(packet) {
        inject(host, event);
    }
}

/// Releases everything the client held, as when it goes away.
pub fn release_all(host: &mut Host) {
    for event in host.input.release_all() {
        inject(host, event);
    }
}

pub fn inject(host: &mut Host, event: InputEvent) {
    tracing::trace!(?event, "inject");
    let time = (host.now_us() / 1000) as u32;
    // Nested, everything goes to the nested compositor's one window; a
    // kiosk app is hit-tested, popups first.
    let s = host.logical_scale();
    let under = |host: &Host, location: Point<f64, Logical>| match &host.kiosk {
        Some(k) => k.surface_under(location),
        None => host.toplevel.as_ref().map(|t| (t.wl_surface().clone(), Point::from((0.0, 0.0)))),
    };
    match event {
        InputEvent::Key { code, pressed } => {
            let Some(kbd) = host.seat.get_keyboard() else { return };
            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
            // evdev codes are XKB keycodes minus 8.
            kbd.input::<(), _>(host, Keycode::new(code + 8), state, SERIAL_COUNTER.next_serial(), time, |_, _, _| {
                FilterResult::Forward
            });
        }
        InputEvent::Button { code, pressed } => {
            let Some(ptr) = host.seat.get_pointer() else { return };
            // A click outside a kiosk app's popups closes them.
            if pressed && let Some(k) = host.kiosk.as_mut() {
                let on_popup = ptr.current_focus().is_some_and(|f| k.popups.find_popup(&f).is_some());
                if !on_popup {
                    k.dismiss_popups();
                }
            }
            let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
            ptr.button(host, &ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time, button: code, state });
            ptr.frame(host);
        }
        InputEvent::PointerAbs { x, y } => {
            let Some(ptr) = host.seat.get_pointer() else { return };
            // The client sends output pixels.
            let (w, h) = (host.layout.width as f64, host.layout.height as f64);
            let location: Point<f64, Logical> =
                (f64::from(x).clamp(0.0, w - 1.0) / s, f64::from(y).clamp(0.0, h - 1.0) / s).into();
            let focus = under(host, location);
            // wlroots' Wayland backend ignores the position in
            // wl_pointer.enter, so the nested compositor's cursor would stay
            // put until the next motion. Follow an enter with a motion.
            let entering = ptr.current_focus().is_none();
            for _ in 0..1 + entering as usize {
                ptr.motion(host, focus.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time });
            }
            ptr.frame(host);
        }
        InputEvent::PointerRel { dx, dy } => {
            let Some(ptr) = host.seat.get_pointer() else { return };
            let delta: Point<f64, _> = (f64::from(dx) / s, f64::from(dy) / s).into();
            let (w, h) = (host.layout.width as f64 / s, host.layout.height as f64 / s);
            let mut location = ptr.current_location() + delta;
            location.x = location.x.clamp(0.0, w - 1.0 / s);
            location.y = location.y.clamp(0.0, h - 1.0 / s);
            let focus = under(host, location);
            let entering = ptr.current_focus().is_none();
            for _ in 0..1 + entering as usize {
                ptr.motion(host, focus.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time });
            }
            let utime = host.now_us();
            ptr.relative_motion(host, focus, &RelativeMotionEvent { delta, delta_unaccel: delta, utime });
            ptr.frame(host);
        }
        InputEvent::Scroll { dx, dy, v120_x, v120_y } => {
            let Some(ptr) = host.seat.get_pointer() else { return };
            let wheel = v120_x != 0 || v120_y != 0;
            let mut frame = AxisFrame::new(time).source(if wheel { AxisSource::Wheel } else { AxisSource::Finger });
            if dx != 0.0 || v120_x != 0 {
                frame = frame.value(Axis::Horizontal, f64::from(dx));
                if v120_x != 0 {
                    frame = frame.v120(Axis::Horizontal, v120_x);
                }
            }
            if dy != 0.0 || v120_y != 0 {
                frame = frame.value(Axis::Vertical, f64::from(dy));
                if v120_y != 0 {
                    frame = frame.v120(Axis::Vertical, v120_y);
                }
            }
            ptr.axis(host, frame);
            ptr.frame(host);
        }
    }
}
