//! The farsight server (Linux only).
//!
//! One process is one session: it creates an isolated environment (its own
//! runtime directory, D-Bus session bus and Wayland display), runs a headless
//! compositor in it, and serves clients on one port. Clients that disconnect
//! can reconnect to the same session. See `docs/design.md` §6.
//!
//! The host compositor runs the desktop nested, encodes it in the format
//! negotiated with the client (§3) and streams it to one client at a time,
//! which sends input back. The desktop is restarted if it crashes
//! (`session`). Commands on stdin drive experiments (see `control`).

mod audio;
mod clipboard;
mod cursor;
mod encode;
mod gpu;
mod host;
mod ime;
mod input;
mod listen;
mod net;
mod outputs;
mod pipeline;
mod session;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use smithay::input::keyboard::XkbConfig;
use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
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
    /// Address to listen on. Default: every address. Required with
    /// --no-tls.
    #[arg(long)]
    listen: Option<std::net::IpAddr>,
    /// Plaintext mode: no encryption or server certificate, for networks
    /// that encrypt and authenticate every packet themselves (Tailscale,
    /// WireGuard). Client keys still apply. Clients must use --no-tls too.
    #[arg(long, requires = "listen")]
    no_tls: bool,
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
    /// Directory holding the server's TLS identity (`cert.der`, `key.der`,
    /// created on first run) and the clients' keys (`authorized_keys`).
    /// Default: `$XDG_CONFIG_HOME/farsight`.
    #[arg(long, alias = "identity")]
    config_dir: Option<PathBuf>,
    /// The client keys allowed in, one `ssh-ed25519 …` line each, as the
    /// desktop client prints them (`farsight-desktop --print-key`).
    /// Default: `authorized_keys` in the config directory.
    #[arg(long)]
    authorized_keys: Option<PathBuf>,
    /// Also write the encoded H.264 elementary stream here.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Constant QP for the encoder.
    #[arg(long, default_value_t = 24)]
    qp: u32,
    /// QP for idle refinement: once the screen is still for 250 ms, it is
    /// sent once more at this QP. Tiles send their JPEG tiles again
    /// losslessly instead. Equal to --qp turns video refinement off.
    #[arg(long, default_value_t = 14)]
    refine_qp: u32,
    /// JPEG quality for tiles, when there is no hardware encoder.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    jpeg_quality: u8,
    /// Hardware encoder backends to offer, best first: vaapi, nvenc. Empty for
    /// tiles only.
    #[arg(long, value_delimiter = ',', default_values_t = ["vaapi".to_string(), "nvenc".to_string()])]
    encoders: Vec<String>,
    /// Read the probe client's frame number from each frame (spike).
    #[arg(long)]
    probe: bool,
    /// When the desktop exits: restart it after a crash (on-failure), always,
    /// or never. A clean exit under on-failure ends the session.
    #[arg(long, value_enum, default_value_t = session::Restart::OnFailure)]
    restart: session::Restart,
    /// Run the session without audio daemons.
    #[arg(long)]
    no_audio: bool,
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
    // Before any thread starts, so that they all inherit the mask.
    let signals = signal_channel()?;

    let mut event_loop: EventLoop<Host> = EventLoop::try_new()?;
    let display: Display<Host> = Display::new()?;
    let dh = display.handle();
    let start = Instant::now();

    let config = match args.config_dir.clone() {
        Some(dir) => dir,
        None => config_dir().context("no $XDG_CONFIG_HOME or $HOME; pass --config-dir")?,
    };
    let identity = farsight_net::endpoint::Identity::load_or_generate(&config)?;
    let authorized_keys = args.authorized_keys.clone().unwrap_or_else(|| config.join("authorized_keys"));
    if !authorized_keys.exists() {
        tracing::warn!(file = %authorized_keys.display(), "no authorized keys yet: no client can connect");
    }
    if args.no_tls {
        let addr = args.listen.expect("clap requires --listen");
        match listen::protected(addr) {
            Some(why) => tracing::info!(%addr, why, "plaintext mode"),
            None => tracing::warn!(
                %addr,
                "PLAINTEXT MODE on a network farsight doesn't recognise as encrypted: the screen, keystrokes \
                 and audio cross it unencrypted and unauthenticated. Use --no-tls only on a tailnet or a \
                 WireGuard tunnel."
            ),
        }
    }
    let (to_host, from_net) = channel::channel();
    let audio = Arc::new(net::Audio::default());
    let net = net::spawn(
        net::Options {
            listen: args.listen,
            plain: args.no_tls,
            port: args.port,
            authorized_keys,
            identity,
            rate_bps: args.rate * 1_000_000,
            start,
            audio: audio.clone(),
        },
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
            refine_qp: args.refine_qp,
            probe: args.probe,
        },
        encoders,
        start,
        net.clone(),
    )?;
    let layout = Layout { width: args.size.0, height: args.size.1, scale: args.scale, refresh_mhz: 60_000 };
    let mut host = Host::new(dh.clone(), event_loop.handle(), start, gpu.renderer, gpu.feedback, pipeline, net, layout);

    let session = session::Session::start(args.port, !args.no_audio)?;
    if session.audio
        && let Err(err) = audio::spawn(session.path("pipewire-0"), start, host.net.clone(), audio)
    {
        tracing::warn!("{err:#}; the session has no audio");
    }
    // The host's socket is in the session's runtime directory, for the
    // desktop only (§6).
    let socket_name = std::ffi::OsString::from("farsight");
    let socket = ListeningSocket::bind_absolute(session.path("farsight")).context("binding the host Wayland socket")?;
    let loop_handle = event_loop.handle();
    loop_handle
        .insert_source(Generic::new(socket, Interest::READ, Mode::Level), |_, socket, host| {
            while let Some(stream) = socket.accept()? {
                if let Err(err) = host.display.insert_client(stream, Arc::new(ClientState::default())) {
                    tracing::warn!(%err, "inserting client");
                }
            }
            Ok(PostAction::Continue)
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
        .insert_source(signals, |event, _, host| {
            if let channel::Event::Msg(signal) = event {
                tracing::info!(signal, "stopping");
                host.running = false;
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

    let mut desktop = session::Desktop::new(args.desktop.clone(), socket_name, args.restart);
    desktop.start(&session)?;
    // Dropped in this order: the desktop, then the services it used.
    let mut supervisor = (desktop, session);
    loop_handle
        .insert_source(Timer::from_duration(Duration::from_millis(250)), move |_, _, host| {
            let (desktop, session) = &mut supervisor;
            session.check_services();
            if desktop.supervise(session) == session::Supervision::End {
                tracing::info!("the session ends");
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
    let _ = host.net.send(net::ToNet::Shutdown("the session ended".into()));
    // Dropping the event loop drops the supervisor, which stops the desktop
    // and the session's services.
    drop(event_loop);
    std::thread::sleep(Duration::from_millis(100));
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
            host.clients.push((id, hello.decoders));
            if hello.view_only {
                tracing::info!(id, "a client joins view-only");
            } else {
                // Taking over: whatever the last client held is let go.
                if host.client.is_some() {
                    input::release_all(host);
                }
                host.client = Some(id);
                host.input = Default::default();
                host.mode = hello.mode;
            }
            renegotiate(host);
            let encodings = host.pipeline.encodings();
            let _ = host.net.send(net::ToNet::Message(id, ServerMessage::Welcome(Welcome { encodings })));
            if !hello.view_only {
                // What a new controlling client needs to know of the
                // session's text, after the welcome.
                if let Some(offer) = host.clipboard.as_ref().map(|c| c.offer()) {
                    let msg = ServerMessage::ClipboardOffer(offer);
                    let _ = host.net.send(net::ToNet::Message(id, msg));
                }
                ime::text_input_changed(host);
            }
            let resized = !hello.view_only && set_layout(host, hello.layout);
            cursor::client_connected(host);
            host.pipeline.set_watched(true);
            // After a resize the desktop's next frame, at the new size, is the
            // first keyframe; otherwise send what is on screen now.
            if !resized {
                pipeline::refresh(host);
            }
        }
        ToHost::Message(id, msg) if host.clients.iter().any(|(c, _)| *c == id) => {
            let controls = host.client == Some(id);
            match msg {
                ClientMessage::RequestKeyframe => {
                    tracing::debug!(id, "keyframe requested");
                    pipeline::refresh(host);
                }
                ClientMessage::RequestRefresh(rects) => {
                    tracing::debug!(?rects, "refresh requested");
                    pipeline::repaint(host, &rects);
                }
                ClientMessage::DecoderFailed(format) => {
                    tracing::warn!(id, %format, "a client's decoder failed");
                    if let Some((_, decoders)) = host.clients.iter_mut().find(|(c, _)| *c == id) {
                        decoders.retain(|d| d.format != format);
                    }
                    host.pipeline.drop_format(format);
                    renegotiate(host);
                    pipeline::refresh(host);
                }
                ClientMessage::SetLayout(layout) if controls => {
                    set_layout(host, layout);
                }
                ClientMessage::SetMode(mode) if controls => {
                    host.mode = mode;
                    // Tiles change their JPEG subsampling; video may change
                    // format.
                    renegotiate(host);
                    pipeline::refresh(host);
                }
                ClientMessage::ClipboardOffer(offer) if controls => {
                    tracing::debug!(mimes = ?offer.mimes, "the client's clipboard changed");
                    if let Some(c) = host.clipboard.as_mut() {
                        c.set_client_offer(&offer);
                    }
                }
                ClientMessage::Text(text) if controls => {
                    if !host.ime.as_mut().is_some_and(|i| i.commit(&text)) {
                        tracing::debug!("text, but no text field in the session takes it");
                    }
                }
                ClientMessage::Preedit { text, cursor } if controls => {
                    if let Some(i) = host.ime.as_mut() {
                        i.preedit(&text, cursor);
                    }
                }
                ClientMessage::SetLayout(_)
                | ClientMessage::SetMode(_)
                | ClientMessage::ClipboardOffer(_)
                | ClientMessage::Text(_)
                | ClientMessage::Preedit { .. } => {
                    tracing::debug!(id, "ignored from a view-only client");
                }
                ClientMessage::Hello(_) => tracing::warn!(id, "second Hello ignored"),
                ClientMessage::SetAudio { .. } => {} // the network thread's
            }
        }
        ToHost::Input(id, packet) if host.client == Some(id) => input::receive(host, &packet),
        ToHost::ClipboardRead(id, request, reply) => match host.clipboard.as_mut() {
            Some(c) if host.client == Some(id) => c.read(request.serial, &request.mime, move |data| {
                let _ = reply.send(data);
            }),
            _ => {
                let _ = reply.send(None);
            }
        },
        ToHost::Disconnected(id) => {
            host.clients.retain(|(c, _)| *c != id);
            if host.client == Some(id) {
                input::release_all(host);
                host.client = None;
            }
            if host.clients.is_empty() {
                host.pipeline.set_watched(false);
            } else {
                // A viewer that limited the format may have gone.
                renegotiate(host);
                pipeline::refresh(host);
            }
        }
        _ => {} // from a connection that has gone, or a viewer's input
    }
    let _ = host.display.flush_clients();
}

/// Picks the formats every client can decode, best first, in the
/// controlling client's mode (§3). The next frame starts a new epoch.
fn renegotiate(host: &mut Host) {
    let lists: Vec<&[farsight_proto::codec::DecoderCaps]> = host.clients.iter().map(|(_, d)| d.as_slice()).collect();
    let decoders = farsight_proto::codec::shared(&lists);
    let choices = pipeline::negotiate(host.pipeline.encoders(), &decoders, host.mode);
    host.pipeline.set_choices(choices, host.mode);
    let encodings = host.pipeline.encodings();
    tracing::info!(
        clients = host.clients.len(), mode = ?host.mode,
        encodings = ?encodings.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "negotiated"
    );
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

/// SIGTERM, SIGINT and SIGHUP, delivered to the event loop so the session
/// ends cleanly. Blocks them in the calling thread, and so in every thread
/// started after.
fn signal_channel() -> anyhow::Result<Channel<i32>> {
    // SAFETY: plain libc calls on a local, initialised sigset.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            anyhow::bail!("blocking signals");
        }
        set
    };
    let (tx, rx) = channel::channel();
    std::thread::Builder::new().name("farsight-signals".into()).spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` is initialised and `signal` a valid out-pointer.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 && tx.send(signal).is_err() {
                break;
            }
        }
    })?;
    Ok(rx)
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
        ["text", ..] => {
            let text = line.trim_start().strip_prefix("text").unwrap_or("").trim_start();
            let taken = host.ime.as_mut().is_some_and(|i| i.commit(text));
            tracing::info!(text, taken, "control: text");
        }
        ["quit"] => host.running = false,
        _ => usage(),
    }
    let _ = host.display.flush_clients();
}

fn usage() {
    tracing::warn!(
        "commands: size W H [S] | scale S | key CODE... | keymap LAYOUT [VARIANT] | move X Y | click [BTN] | keyframe | text TEXT | quit"
    );
}
