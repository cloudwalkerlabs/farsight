//! The farsight server (Linux only).
//!
//! One process is one session: it creates an isolated environment (its own
//! runtime directory, D-Bus session bus and Wayland display), runs a headless
//! compositor in it, and serves clients on one port. Clients that disconnect
//! can reconnect to the same session. See `docs/design.md` §6.
//!
//! Current state: the M0 spike. The host compositor runs the desktop nested,
//! encodes it with VA-API and writes H.264 to a file. Spike experiments are
//! driven by commands on stdin (see `control`).

mod encode;
mod gpu;
mod host;
mod pipeline;

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::socket::ListeningSocketSource;
use tracing_subscriber::EnvFilter;

use host::{ClientState, Host, Layout};

#[derive(Parser)]
#[command(version, about = "farsight server: a headless Wayland session served over QUIC")]
struct Args {
    /// UDP port to listen on.
    #[arg(short, long, default_value_t = farsight_proto::DEFAULT_PORT)]
    port: u16,
    /// Render node for compositing and encoding.
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: PathBuf,
    /// Initial output size in physical pixels, WIDTHxHEIGHT.
    #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
    size: (i32, i32),
    /// Initial output scale.
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    /// Write the encoded H.264 elementary stream here (spike).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Constant QP for the spike's encoder.
    #[arg(long, default_value_t = 24)]
    qp: u32,
    /// Read the probe client's frame number from each frame (spike).
    #[arg(long)]
    probe: bool,
    /// The desktop to run nested.
    #[arg(last = true, default_values_t = ["labwc".to_string()])]
    desktop: Vec<String>,
}

fn parse_size(s: &str) -> Result<(i32, i32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    Ok((w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?))
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(port = args.port, "farsight-server starting");

    let mut event_loop: EventLoop<Host> = EventLoop::try_new()?;
    let display: Display<Host> = Display::new()?;
    let dh = display.handle();

    let gpu = gpu::open(&args.render_node)?;
    let pipeline = pipeline::Pipeline::new(pipeline::Options {
        render_node: args.render_node.clone(),
        out: args.out.clone(),
        qp: args.qp,
        probe: args.probe,
    })?;
    let layout = Layout { width: args.size.0, height: args.size.1, scale: args.scale };
    let mut host = Host::new(dh.clone(), gpu.renderer, gpu.feedback, pipeline, layout);

    let socket = ListeningSocketSource::with_name(&format!("farsight-{}", args.port))
        .context("binding the host Wayland socket")?;
    let socket_name = socket.socket_name().to_os_string();
    let loop_handle = event_loop.handle();
    loop_handle
        .insert_source(socket, |stream, _, host| {
            if let Err(err) = host.display.insert_client(stream, Arc::new(ClientState::default())) {
                tracing::warn!(%err, "inserting client");
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, host| {
                // SAFETY: the display is not dropped while the source exists.
                unsafe { display.get_mut().dispatch_clients(host).unwrap() };
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    loop_handle
        .insert_source(control_channel(), |event, _, host| {
            if let channel::Event::Msg(line) = event {
                control(host, &line);
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut child = spawn_desktop(&args.desktop, &socket_name)?;
    let child_pid = child.id();
    loop_handle
        .insert_source(Timer::from_duration(Duration::from_millis(250)), move |_, _, host| {
            if let Ok(Some(status)) = child.try_wait() {
                tracing::info!(%status, "desktop exited");
                host.running = false;
                return TimeoutAction::Drop;
            }
            TimeoutAction::ToDuration(Duration::from_millis(250))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    while host.running {
        event_loop.dispatch(Some(Duration::from_millis(100)), &mut host)?;
        let _ = host.display.flush_clients();
    }
    // SAFETY: plain syscall; the child leads its own process group.
    unsafe { libc::kill(-(child_pid as i32), libc::SIGTERM) };
    Ok(())
}

fn spawn_desktop(cmd: &[String], socket: &std::ffi::OsStr) -> anyhow::Result<Child> {
    tracing::info!(?cmd, ?socket, "starting desktop");
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .process_group(0);
    // The session's environment comes from us, not from whoever started the
    // server (§6): a stray WLR_BACKENDS would stop labwc nesting.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"WLR_") {
            c.env_remove(key);
        }
    }
    c.spawn().with_context(|| format!("starting {}", cmd[0]))
}

/// Spike experiments, one command per line on stdin.
fn control_channel() -> Channel<String> {
    let (tx, rx) = channel::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn control(host: &mut Host, line: &str) {
    let words: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| words.get(i).and_then(|w| w.parse::<f64>().ok());
    let time = (host.now_us() / 1000) as u32;
    match words.as_slice() {
        ["size", ..] | ["layout", ..] => {
            let (Some(w), Some(h)) = (num(1), num(2)) else { return usage() };
            let scale = num(3).unwrap_or(host.layout.scale);
            tracing::info!(w, h, scale, "control: layout");
            host.apply_output(Layout { width: w as i32, height: h as i32, scale });
        }
        ["scale", _] => {
            let Some(scale) = num(1) else { return usage() };
            tracing::info!(scale, "control: scale");
            host.apply_output(Layout { scale, ..host.layout });
        }
        ["key", ..] => {
            // evdev keycodes, pressed in order and released in reverse.
            let codes: Vec<u32> = words[1..].iter().filter_map(|w| w.parse().ok()).collect();
            let Some(kbd) = host.seat.get_keyboard() else { return };
            for (codes, state) in [
                (codes.clone(), smithay::backend::input::KeyState::Pressed),
                (codes.into_iter().rev().collect(), smithay::backend::input::KeyState::Released),
            ] {
                for code in codes {
                    let serial = SERIAL_COUNTER.next_serial();
                    kbd.input::<(), _>(host, Keycode::new(code + 8), state, serial, time, |_, _, _| {
                        FilterResult::Forward
                    });
                }
            }
        }
        ["keymap", layout, rest @ ..] => {
            let variant = rest.first().copied().unwrap_or("");
            let Some(kbd) = host.seat.get_keyboard() else { return };
            let config = XkbConfig { layout, variant, ..Default::default() };
            match kbd.set_xkb_config(host, config) {
                Ok(()) => tracing::info!(layout, variant, "control: keymap"),
                Err(err) => tracing::warn!(?err, "keymap"),
            }
        }
        ["move", ..] => {
            let (Some(x), Some(y)) = (num(1), num(2)) else { return usage() };
            let Some(ptr) = host.seat.get_pointer() else { return };
            let focus = host.toplevel.as_ref().map(|t| (t.wl_surface().clone(), (0.0, 0.0).into()));
            let event = MotionEvent { location: (x, y).into(), serial: SERIAL_COUNTER.next_serial(), time };
            ptr.motion(host, focus, &event);
            ptr.frame(host);
        }
        ["click", ..] => {
            let button = num(1).map(|b| b as u32).unwrap_or(0x110); // BTN_LEFT
            let Some(ptr) = host.seat.get_pointer() else { return };
            for state in [
                smithay::backend::input::ButtonState::Pressed,
                smithay::backend::input::ButtonState::Released,
            ] {
                let event = ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time, button, state };
                ptr.button(host, &event);
                ptr.frame(host);
            }
        }
        ["quit"] => host.running = false,
        _ => usage(),
    }
    let _ = host.display.flush_clients();
}

fn usage() {
    tracing::warn!(
        "commands: size W H [S] | scale S | key CODE... | keymap LAYOUT [VARIANT] | move X Y | click [BTN] | quit"
    );
}
