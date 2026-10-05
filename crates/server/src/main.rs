//! The farsight server (Linux only).
//!
//! One process is one session: it creates an isolated environment (its own
//! runtime directory, D-Bus session bus and Wayland display), runs a headless
//! compositor in it, and serves clients on one port. Clients that disconnect
//! can reconnect to the same session. See `docs/design.md` §6.
//!
//! Current state: M2. The host compositor runs the desktop nested, encodes
//! it in the format negotiated with the client (§3) and streams it to one
//! client at a time, which sends input back. The session's isolation (§6)
//! comes in M3. Commands on stdin drive experiments (see `control`).

mod cursor;
mod encode;
mod gpu;
mod host;
mod input;
mod net;
mod pipeline;

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use smithay::input::keyboard::XkbConfig;
use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;
use tracing_subscriber::EnvFilter;

use farsight_proto::control::{ClientMessage, ServerMessage, Welcome};
use farsight_proto::input::InputEvent;
use host::{ClientState, Host, Layout};
use net::ToHost;

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
    /// Pace video at this many Mbit/s. A stand-in for congestion control,
    /// which comes in M4.
    #[arg(long, default_value_t = 100)]
    rate: u64,
    /// Directory holding the server's TLS identity (`cert.der`, `key.der`);
    /// created on first run. Default: `$XDG_CONFIG_HOME/farsight`.
    #[arg(long)]
    identity: Option<PathBuf>,
    /// Also write the encoded H.264 elementary stream here.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Constant QP for the encoder.
    #[arg(long, default_value_t = 24)]
    qp: u32,
    /// JPEG quality for tiles, when there is no hardware encoder.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    jpeg_quality: u8,
    /// Hardware encoder backends to offer, best first: vaapi. Empty for
    /// tiles only.
    #[arg(long, value_delimiter = ',', default_values_t = ["vaapi".to_string()])]
    encoders: Vec<String>,
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
    let start = Instant::now();

    let identity_dir = match args.identity.clone() {
        Some(dir) => dir,
        None => config_dir().context("no $XDG_CONFIG_HOME or $HOME; pass --identity")?,
    };
    let identity = farsight_net::endpoint::Identity::load_or_generate(&identity_dir)?;
    let (to_host, from_net) = channel::channel();
    let net = net::spawn(
        net::Options { port: args.port, identity, rate_bps: args.rate * 1_000_000, start },
        to_host,
    )?;

    let gpu = gpu::open(&args.render_node)?;
    let backends = args
        .encoders
        .iter()
        .filter(|b| !b.is_empty())
        .map(|b| b.parse::<encode::Backend>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::msg)?;
    let encoders = encode::probe(&args.render_node, &backends);
    for e in &encoders {
        tracing::info!(
            format = %e.caps.format, backend = ?e.backend, max = ?(e.caps.max_width, e.caps.max_height),
            "encoder available"
        );
    }
    let pipeline = pipeline::Pipeline::new(
        pipeline::Options {
            render_node: args.render_node.clone(),
            out: args.out.clone(),
            qp: args.qp,
            jpeg_quality: args.jpeg_quality,
            probe: args.probe,
        },
        encoders,
        start,
        net.clone(),
    )?;
    let layout = Layout { width: args.size.0, height: args.size.1, scale: args.scale, refresh_mhz: 60_000 };
    let mut host = Host::new(dh.clone(), event_loop.handle(), start, gpu.renderer, gpu.feedback, pipeline, net, layout);

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
        .insert_source(from_net, |event, _, host| {
            if let channel::Event::Msg(msg) = event {
                on_net(host, msg);
            }
        })
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

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("farsight"))
}

/// Messages from the network thread.
fn on_net(host: &mut Host, msg: ToHost) {
    match msg {
        ToHost::Connected(id, hello) => {
            if host.client.is_some() {
                input::release_all(host);
            }
            let choices = pipeline::negotiate(host.pipeline.encoders(), &hello.decoders, hello.mode);
            host.client = Some(id);
            host.input = Default::default();
            host.decoders = hello.decoders;
            host.mode = hello.mode;
            host.pipeline.set_choices(choices, hello.mode);
            let encodings = host.pipeline.encodings();
            tracing::info!(id, mode = ?hello.mode, encodings = ?encodings.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "negotiated");
            let _ = host.net.send(net::ToNet::Message(id, ServerMessage::Welcome(Welcome { encodings })));
            // Resizing mid-session is M2; the first layout is applied as is.
            let resized = set_layout(host, hello.layout);
            cursor::client_connected(host);
            host.pipeline.set_client(Some(id));
            // After a resize the desktop's next frame, at the new size, is the
            // first keyframe; otherwise send what is on screen now.
            if !resized {
                pipeline::refresh(host);
            }
        }
        ToHost::Message(id, msg) if host.client == Some(id) => match msg {
            ClientMessage::RequestKeyframe => {
                tracing::debug!("keyframe requested");
                pipeline::refresh(host);
            }
            ClientMessage::SetLayout(layout) => tracing::info!(?layout, "SetLayout ignored until M2"),
            ClientMessage::RequestRefresh(rects) => {
                tracing::debug!(?rects, "refresh requested");
                pipeline::repaint(host, &rects);
            }
            ClientMessage::SetMode(mode) => {
                host.mode = mode;
                let choices = pipeline::negotiate(host.pipeline.encoders(), &host.decoders, mode);
                tracing::info!(?mode, formats = ?choices.iter().map(|c| c.format.to_string()).collect::<Vec<_>>(), "mode changed");
                // Tiles change their JPEG subsampling; video may change format.
                host.pipeline.set_choices(choices, mode);
                pipeline::refresh(host);
            }
            ClientMessage::DecoderFailed(format) => {
                tracing::warn!(%format, "the client's decoder failed");
                host.decoders.retain(|d| d.format != format);
                host.pipeline.drop_format(format);
                pipeline::refresh(host);
            }
            ClientMessage::Hello(_) => tracing::warn!(id, "second Hello ignored"),
        },
        ToHost::Input(id, packet) if host.client == Some(id) => input::receive(host, &packet),
        ToHost::Disconnected(id) if host.client == Some(id) => {
            input::release_all(host);
            host.client = None;
            host.pipeline.set_client(None);
        }
        _ => {} // from a connection that has been taken over
    }
    let _ = host.display.flush_clients();
}

/// Applies the client's layout, within what the encoder allows. Returns
/// whether the output changed.
fn set_layout(host: &mut Host, l: farsight_proto::layout::Layout) -> bool {
    let l = l.constrained(host.pipeline.max_size());
    let layout = Layout {
        width: l.width_px as i32,
        height: l.height_px as i32,
        scale: l.scale_120 as f64 / farsight_proto::layout::SCALE_DENOMINATOR as f64,
        refresh_mhz: if l.refresh_mhz == 0 { 60_000 } else { l.refresh_mhz },
    };
    if layout == host.layout {
        return false;
    }
    tracing::info!(?layout, "applying the client's layout");
    host.apply_output(layout);
    true
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

/// Experiments, one command per line on stdin.
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
    match words.as_slice() {
        ["size", ..] | ["layout", ..] => {
            let (Some(w), Some(h)) = (num(1), num(2)) else { return usage() };
            let scale = num(3).unwrap_or(host.layout.scale);
            tracing::info!(w, h, scale, "control: layout");
            host.apply_output(Layout { width: w as i32, height: h as i32, scale, ..host.layout });
        }
        ["scale", _] => {
            let Some(scale) = num(1) else { return usage() };
            tracing::info!(scale, "control: scale");
            host.apply_output(Layout { scale, ..host.layout });
        }
        ["key", ..] => {
            // evdev keycodes, pressed in order and released in reverse.
            let codes: Vec<u32> = words[1..].iter().filter_map(|w| w.parse().ok()).collect();
            for &code in &codes {
                input::inject(host, InputEvent::Key { code, pressed: true });
            }
            for &code in codes.iter().rev() {
                input::inject(host, InputEvent::Key { code, pressed: false });
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
            input::inject(host, InputEvent::PointerAbs { x: x as f32, y: y as f32 });
        }
        ["click", ..] => {
            let code = num(1).map(|b| b as u32).unwrap_or(0x110); // BTN_LEFT
            input::inject(host, InputEvent::Button { code, pressed: true });
            input::inject(host, InputEvent::Button { code, pressed: false });
        }
        ["keyframe"] => pipeline::refresh(host),
        ["quit"] => host.running = false,
        _ => usage(),
    }
    let _ = host.display.flush_clients();
}

fn usage() {
    tracing::warn!(
        "commands: size W H [S] | scale S | key CODE... | keymap LAYOUT [VARIANT] | move X Y | click [BTN] | keyframe | quit"
    );
}
