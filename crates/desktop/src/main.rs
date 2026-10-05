//! The desktop client.
//!
//! Three threads: the window and GL on the main thread (winit), the
//! connection on a tokio runtime (`farsight-client`), and the decoder on its
//! own thread. Frames go straight from the connection to the decoder; only
//! the newest decoded picture is drawn, and drawing doesn't wait for vsync.
//! The remote cursor is the window's own cursor, so moving it has no
//! latency (§4).
//!
//! The window is the source of truth for the remote output (§5): its size
//! in physical pixels and its scale go to the server whenever they change,
//! at most every 50 ms while the user drags. Until the server's picture
//! matches, the last one is stretched to the window.

mod audio;
mod cursor;
mod decode;
mod render;
mod stats;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::num::NonZeroU32;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::Parser;
use farsight_client::{Client, Config, Event, TilesPacket, VideoFrame};
use farsight_proto::codec::{Encoding, Format, Mode};
use farsight_proto::control::{CursorImage, CursorShape};
use farsight_proto::input::InputEvent;
use farsight_proto::layout::{Layout, SCALE_DENOMINATOR};
use farsight_proto::tiles::Rect;
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::prelude::{GlSurface, NotCurrentGlContext};
use glutin::surface::{Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{CursorIcon, CustomCursor, Window, WindowId};

use decode::{Decoder, Picture};
use render::{Placement, Renderer};

#[derive(Parser)]
#[command(version, about = "farsight desktop client")]
struct Args {
    /// Server address, `host[:port]`.
    #[arg(required_unless_present = "print_key")]
    address: Option<String>,
    /// Print this client's public key, as a line for the server's
    /// `authorized_keys`, and exit.
    #[arg(long)]
    print_key: bool,
    /// Directory holding this client's key (`client_key`) and the servers
    /// it knows (`known_hosts`). Default: `$XDG_CONFIG_HOME/farsight`.
    #[arg(long)]
    config_dir: Option<std::path::PathBuf>,
    /// Initial window size in logical pixels, WIDTHxHEIGHT.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    size: (u32, u32),
    /// Decode in software even if VA-API is available.
    #[arg(long)]
    software: bool,
    /// What matters most: `text` (sharp text, 4:4:4 where it can) or
    /// `motion`.
    #[arg(long, default_value = "text", value_parser = parse_mode)]
    mode: Mode,
    /// Offer only these formats, e.g. `hevc,h264:444`.
    #[arg(long, value_delimiter = ',')]
    codec: Vec<String>,
    /// Don't play the session's audio; the server doesn't send it.
    #[arg(long)]
    no_audio: bool,
    /// Watch only, next to whoever controls the session, instead of taking
    /// it over. Input and the window's size aren't sent.
    #[arg(long)]
    view_only: bool,
    /// Plaintext mode, for a server started with --no-tls on a tailnet or
    /// WireGuard tunnel. A server once reached over TLS is refused in
    /// plaintext until its known_hosts line is removed.
    #[arg(long)]
    no_tls: bool,
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    match s {
        "text" => Ok(Mode::Text),
        "motion" => Ok(Mode::Motion),
        _ => Err("expected text or motion".into()),
    }
}

/// `h264` is short for `h264:420`.
fn format_matches(format: Format, name: &str) -> bool {
    let full = format.to_string();
    full == name || full.strip_suffix(":420") == Some(name)
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    Ok((w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?))
}

fn default_config_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("farsight"))
}

/// `user@host`, to tell keys apart in `authorized_keys`.
fn key_comment() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    format!("{user}@{}", host.trim())
}

/// The address with its port, as the server is known in `known_hosts`.
fn server_name(address: &str) -> String {
    let port = farsight_proto::DEFAULT_PORT;
    if address.parse::<SocketAddr>().is_ok() {
        return address.to_string();
    }
    if let Ok(ip) = address.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        return SocketAddr::from((ip, port)).to_string();
    }
    match address.rsplit_once(':') {
        Some((_, p)) if p.parse::<u16>().is_ok() => address.to_string(),
        _ => format!("{address}:{port}"),
    }
}

fn resolve(address: &str) -> anyhow::Result<SocketAddr> {
    server_name(address).to_socket_addrs()?.next().with_context(|| format!("{address} has no address"))
}

/// From the other threads to the window.
enum UserEvent {
    Connected(Arc<Client>),
    /// Connecting failed; `refused` if trying again won't help.
    Failed { error: String, refused: bool },
    Net(Event),
    Decoded(Decoded),
}

struct Decoded {
    content: Content,
    /// The server's commit time and its encode time, from the header.
    capture_us: u64,
    encode_us: u32,
    complete_us: u64,
    decoded_us: u64,
    /// Counts in the statistics: every video frame, and the last datagram
    /// of each tiles update.
    sample: bool,
}

struct Gfx {
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    renderer: Renderer,
}

struct App {
    args: Args,
    addr: SocketAddr,
    /// `host:port`, as pinned in `known_hosts`.
    server_name: String,
    config_dir: std::path::PathBuf,
    key: Arc<farsight_net::auth::ClientKey>,
    proxy: EventLoopProxy<UserEvent>,
    runtime: tokio::runtime::Runtime,
    gfx: Option<Gfx>,
    client: Option<Arc<Client>>,
    /// Set once connected, for the decode thread.
    client_cell: Arc<OnceLock<Arc<Client>>>,
    /// The newest decoded picture, not drawn yet.
    pending: Option<Decoded>,
    /// Tiles not drawn yet: all of them, each only once.
    pending_tiles: Vec<Decoded>,
    placement: Option<Placement>,
    cursors: HashMap<u64, CustomCursor>,
    stats: stats::Latency,
    decode_thread: Option<std::thread::JoinHandle<()>>,
    exit: Option<anyhow::Error>,
    /// The layout last sent, and when.
    layout_sent: Option<(Layout, Instant)>,
    /// A layout waiting for the throttle.
    layout_pending: Option<Layout>,
    /// Audio announced before the connection was ready.
    audio_pending: Option<farsight_proto::audio::AudioConfig>,
    audio: Option<audio::Output>,
    /// The connection was lost; connect again at this time.
    reconnect_at: Option<Instant>,
    /// Failed attempts since the connection was lost; `None` while
    /// connected, or before the first connection.
    reconnects: Option<u32>,
}

/// The wait before reconnecting: 0.5 s, doubling to 5 s.
fn reconnect_delay(attempts: u32) -> Duration {
    Duration::from_millis((500u64 << attempts.min(4)).min(5000))
}

/// At most one `SetLayout` this often during a drag-resize (§5).
const LAYOUT_INTERVAL: Duration = Duration::from_millis(50);

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let config_dir = match &args.config_dir {
        Some(d) => d.clone(),
        None => default_config_dir().context("no $XDG_CONFIG_HOME or $HOME; pass --config-dir")?,
    };
    let key = Arc::new(farsight_net::auth::ClientKey::load_or_generate(&config_dir.join("client_key"))?);
    if args.print_key {
        println!("{}", key.authorized_line(&key_comment()));
        return Ok(());
    }
    let address = args.address.clone().expect("required by clap");
    let addr = resolve(&address)?;
    let server_name = server_name(&address);
    tracing::info!(%addr, core = farsight_client::version(), "farsight desktop client");

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let mut app = App {
        args,
        addr,
        server_name,
        config_dir,
        key,
        proxy: event_loop.create_proxy(),
        runtime,
        gfx: None,
        client: None,
        client_cell: Arc::default(),
        pending: None,
        pending_tiles: Vec::new(),
        placement: None,
        cursors: HashMap::new(),
        stats: stats::Latency::default(),
        decode_thread: None,
        exit: None,
        layout_sent: None,
        layout_pending: None,
        audio_pending: None,
        audio: None,
        reconnect_at: None,
        reconnects: None,
    };
    event_loop.run_app(&mut app)?;
    let App { client, runtime, decode_thread, exit, .. } = app;
    if let Some(client) = client {
        client.close();
    }
    // Ending the runtime drops the connection's callback, which ends the
    // decode thread. Wait for it: the iHD driver's exit-time destructors
    // crash while a VA context is still open.
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Some(t) = decode_thread {
        let _ = t.join();
    }
    match exit {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        self.exit = Some(err);
        event_loop.exit();
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let attrs = Window::default_attributes()
            .with_title(format!("farsight — {}", self.server_name))
            .with_inner_size(winit::dpi::LogicalSize::new(self.args.size.0, self.args.size.1));
        let (window, config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, ConfigTemplateBuilder::new(), |configs| {
                configs.reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a }).unwrap()
            })
            .map_err(|e| anyhow::anyhow!("creating the window: {e}"))?;
        let window = window.context("no window")?;
        let display = config.display();
        let raw = window.window_handle()?.as_raw();
        let ctx_attrs =
            ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(Some(Version::new(3, 0)))).build(Some(raw));
        // SAFETY: the window outlives the context and surface (both in Gfx,
        // dropped before it).
        let context = unsafe { display.create_context(&config, &ctx_attrs)? };
        let surface_attrs = window.build_surface_attributes(Default::default())?;
        // SAFETY: as above.
        let surface = unsafe { display.create_window_surface(&config, &surface_attrs)? };
        let context = context.make_current(&surface)?;
        // Present at once; the compositor shows the newest at its next
        // refresh, like a mailbox.
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            tracing::warn!(%err, "can't turn vsync off");
        }
        // SAFETY: the context is current on this thread.
        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
        let renderer = Renderer::new(gl)?;
        self.gfx = Some(Gfx { window, surface, context, renderer });
        Ok(())
    }

    fn connect(&mut self) {
        let Some(layout) = self.window_layout() else { return };
        self.layout_sent = Some((layout, Instant::now()));
        // A new connection gets a decoder thread of its own; the last one
        // has ended with its connection. One VA context at a time.
        if let Some(t) = self.decode_thread.take() {
            let _ = t.join();
        }
        self.client_cell = Arc::default();
        let (decode_tx, decode_rx) = mpsc::channel::<ToDecoder>();
        let mut decoders = decode::offered(!self.args.software);
        if !self.args.codec.is_empty() {
            decoders.retain(|d| self.args.codec.iter().any(|c| format_matches(d.format, c)));
        }
        for d in &decoders {
            tracing::info!(format = %d.format, hardware = d.hardware, max = ?(d.max_width, d.max_height), "decoder available");
        }
        {
            let (proxy, cell, hardware) = (self.proxy.clone(), self.client_cell.clone(), !self.args.software);
            let thread = std::thread::Builder::new()
                .name("farsight-decode".into())
                .spawn(move || decode_thread(hardware, decode_rx, proxy, cell))
                .expect("spawning the decode thread");
            self.decode_thread = Some(thread);
        }
        let audio = (!self.args.no_audio).then_some(farsight_proto::audio::AudioCaps { max_channels: 2 });
        let cfg = Config {
            addr: self.addr,
            server_name: self.server_name.clone(),
            key: self.key.clone(),
            known_hosts: farsight_net::auth::KnownHosts::new(self.config_dir.join("known_hosts")),
            plain: self.args.no_tls,
            layout,
            decoders,
            mode: self.args.mode,
            audio,
            view_only: self.args.view_only,
        };
        let (proxy, cell) = (self.proxy.clone(), self.client_cell.clone());
        let events = self.proxy.clone();
        tracing::info!(?layout, "connecting");
        self.runtime.spawn(async move {
            let on_event = move |event| match event {
                Event::Frame(f) => {
                    let _ = decode_tx.send(ToDecoder::Frame(f));
                }
                Event::Epoch(e) => {
                    let _ = decode_tx.send(ToDecoder::Epoch(e.epoch, e.encoding, (e.layout.width_px, e.layout.height_px)));
                }
                Event::Tiles(t) => {
                    let _ = decode_tx.send(ToDecoder::Tiles(t));
                }
                Event::Connected { .. } => {}
                other => {
                    let _ = events.send_event(UserEvent::Net(other));
                }
            };
            match Client::connect(cfg, on_event).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    let _ = cell.set(client.clone());
                    let _ = proxy.send_event(UserEvent::Connected(client));
                }
                Err(err) => {
                    let refused = err.is::<farsight_client::Refused>();
                    let _ = proxy.send_event(UserEvent::Failed { error: format!("{err:#}"), refused });
                }
            }
        });
    }

    /// The window's size and scale, as the server should apply them.
    fn window_layout(&self) -> Option<Layout> {
        let gfx = self.gfx.as_ref()?;
        let size = gfx.window.inner_size();
        Some(Layout {
            width_px: size.width,
            height_px: size.height,
            scale_120: (gfx.window.scale_factor() * SCALE_DENOMINATOR as f64).round() as u32,
            refresh_mhz: gfx.window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).unwrap_or(60_000),
        })
    }

    /// The window changed size or scale: tell the server, now or once the
    /// throttle allows.
    fn layout_changed(&mut self) {
        let Some(layout) = self.window_layout() else { return };
        if layout.width_px == 0 || layout.height_px == 0 {
            return;
        }
        if self.layout_sent.is_some_and(|(l, _)| l == layout) {
            self.layout_pending = None;
            return;
        }
        self.layout_pending = Some(layout);
        self.send_layout();
    }

    /// Sends the pending layout if the throttle allows; returns when it
    /// will, otherwise.
    fn send_layout(&mut self) -> Option<Instant> {
        if self.args.view_only {
            return None; // the controlling client's window sets the layout
        }
        let layout = self.layout_pending?;
        let client = self.client.as_ref()?;
        if let Some((_, at)) = self.layout_sent
            && at.elapsed() < LAYOUT_INTERVAL
        {
            return Some(at + LAYOUT_INTERVAL);
        }
        tracing::info!(?layout, "SetLayout");
        client.set_layout(layout);
        self.layout_sent = Some((layout, Instant::now()));
        self.layout_pending = None;
        None
    }

    fn start_audio(&mut self, config: farsight_proto::audio::AudioConfig) {
        let Some(client) = self.client.clone() else { return };
        self.audio = None;
        match audio::open(client.clone(), config) {
            Ok(output) => {
                self.audio = Some(output);
                self.runtime.spawn(audio::report(client));
            }
            Err(err) => {
                tracing::warn!("{err:#}; muting the session's audio");
                client.set_audio(false);
            }
        }
    }

    fn set_title(&self, suffix: &str) {
        if let Some(gfx) = &self.gfx {
            gfx.window.set_title(&format!("farsight — {}{suffix}", self.server_name));
        }
    }

    fn input(&self, event: InputEvent) {
        if self.args.view_only {
            return;
        }
        if let Some(c) = &self.client {
            c.input(event);
        }
    }

    fn set_cursor(&mut self, shape: CursorShape) {
        let Some(gfx) = &self.gfx else { return };
        match shape {
            CursorShape::Hidden => gfx.window.set_cursor_visible(false),
            CursorShape::Named(name) => {
                gfx.window.set_cursor(name.parse::<CursorIcon>().unwrap_or_default());
                gfx.window.set_cursor_visible(true);
            }
            CursorShape::Image(id) => {
                if let Some(c) = self.cursors.get(&id) {
                    gfx.window.set_cursor(c.clone());
                    gfx.window.set_cursor_visible(true);
                } else {
                    tracing::warn!(id, "cursor image never arrived");
                }
            }
        }
    }

    fn add_cursor_image(&mut self, event_loop: &ActiveEventLoop, image: CursorImage) {
        tracing::debug!(
            id = image.id, size = ?(image.width, image.height), hotspot = ?image.hotspot, scale_120 = image.scale_120,
            "cursor image"
        );
        // The remote output's pixels are the window's physical pixels, and
        // winit takes cursors in physical pixels: draw the image at its
        // size on the remote output.
        let image = cursor::to_output_pixels(image);
        // winit wants straight alpha; the server sends premultiplied BGRA.
        let mut rgba = image.pixels;
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            let un = |c: u8| (c as u32 * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
            let (b, g, r) = (px[0], px[1], px[2]);
            px[0] = un(r);
            px[1] = un(g);
            px[2] = un(b);
        }
        let hotspot =
            (image.hotspot.0.clamp(0, image.width as i32 - 1), image.hotspot.1.clamp(0, image.height as i32 - 1));
        match CustomCursor::from_rgba(rgba, image.width as u16, image.height as u16, hotspot.0 as u16, hotspot.1 as u16) {
            Ok(source) => {
                self.cursors.insert(image.id, event_loop.create_custom_cursor(source));
            }
            Err(err) => tracing::warn!(%err, "bad cursor image"),
        }
    }

    fn redraw(&mut self) {
        let Some(gfx) = &mut self.gfx else { return };
        let mut pending = self.pending.take();
        if let Some(Decoded { content: Content::Picture(picture), .. }) = &pending {
            gfx.renderer.upload(picture);
        }
        for d in std::mem::take(&mut self.pending_tiles) {
            if let Content::Tiles { size, tiles } = &d.content {
                gfx.renderer.upload_tiles(*size, tiles);
            }
            if d.sample {
                pending = Some(d);
            }
        }
        let size = gfx.window.inner_size();
        self.placement = gfx.renderer.draw((size.width, size.height));
        if let Err(err) = gfx.surface.swap_buffers(&gfx.context) {
            tracing::warn!(%err, "swap");
        }
        if let (Some(d), Some(client)) = (pending, &self.client) {
            let presented_us = client.now_us();
            if d.decoded_us > 0
                && let Some(capture) = client.server_to_local(d.capture_us)
            {
                self.stats.add(stats::Sample {
                    encode_us: d.encode_us as u64,
                    capture_local_us: capture,
                    complete_us: d.complete_us,
                    decoded_us: d.decoded_us,
                    presented_us,
                });
            }
            self.stats.maybe_report(presented_us, || client.stats());
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    /// The GL surface must go while the Wayland connection is still up:
    /// NVIDIA's EGL crashes destroying it afterwards.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.audio = None;
        self.gfx = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.reconnect_at.is_some_and(|at| Instant::now() >= at) {
            self.reconnect_at = None;
            self.connect();
        }
        let wake = [self.send_layout(), self.reconnect_at].into_iter().flatten().min();
        match wake {
            Some(at) => event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait),
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        if let Err(err) = self.create_window(event_loop) {
            return self.fail(event_loop, err);
        }
        self.connect();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Connected(client) => {
                self.client = Some(client);
                if self.reconnects.take().is_some() {
                    tracing::info!("reconnected");
                    self.set_title("");
                }
                if let Some(config) = self.audio_pending.take() {
                    self.start_audio(config);
                }
                // The window may have changed while connecting.
                self.layout_changed();
            }
            UserEvent::Failed { error, refused } => match self.reconnects {
                // Lost, and not back yet: keep trying, unless refused.
                Some(n) if !refused => {
                    tracing::info!(attempt = n + 1, "reconnecting failed: {error}");
                    self.reconnects = Some(n + 1);
                    self.reconnect_at = Some(Instant::now() + reconnect_delay(n + 1));
                }
                _ => self.fail(event_loop, anyhow::anyhow!(error)),
            },
            UserEvent::Decoded(d) => {
                if let Some(gfx) = &self.gfx {
                    gfx.window.request_redraw();
                }
                match d.content {
                    Content::Picture(_) => self.pending = Some(d),
                    Content::Tiles { .. } => self.pending_tiles.push(d),
                }
            }
            UserEvent::Net(Event::CursorImage(image)) => self.add_cursor_image(event_loop, image),
            UserEvent::Net(Event::Cursor(shape)) => self.set_cursor(shape),
            UserEvent::Net(Event::AudioConfig(config)) => match self.client {
                Some(_) => self.start_audio(config),
                None => self.audio_pending = Some(config),
            },
            UserEvent::Net(Event::Closed { reason, retry }) => {
                tracing::info!(%reason, "disconnected");
                self.client = None;
                self.audio = None;
                self.audio_pending = None;
                if retry {
                    // The session lives on at the server; the picture stays
                    // until it's back.
                    self.reconnects = Some(0);
                    self.reconnect_at = Some(Instant::now() + reconnect_delay(0));
                    self.set_title(" (reconnecting…)");
                } else {
                    println!("{reason}");
                    event_loop.exit();
                }
            }
            UserEvent::Net(_) => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let (Some(gfx), Some(w), Some(h)) =
                    (&self.gfx, NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    gfx.surface.resize(&gfx.context, w, h);
                    gfx.window.request_redraw();
                }
                self.layout_changed();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.layout_changed(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Focused(false) => {
                if let Some(c) = &self.client {
                    c.release_all();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // The nested compositor's apps repeat keys themselves.
                if event.repeat {
                    return;
                }
                // On Linux, winit's scancode is the evdev code.
                if let Some(code) = event.physical_key.to_scancode() {
                    self.input(InputEvent::Key { code, pressed: event.state == ElementState::Pressed });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(p) = self.placement {
                    let (x, y) = p.to_picture(position.x, position.y);
                    self.input(InputEvent::PointerAbs { x: x as f32, y: y as f32 });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let code = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x113,
                    MouseButton::Forward => 0x114,
                    MouseButton::Other(_) => return,
                };
                self.input(InputEvent::Button { code, pressed: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Wayland's axes point down and right; winit's point up and
                // left. One wheel click is 15 px, as in libinput.
                let event = match delta {
                    MouseScrollDelta::LineDelta(x, y) => InputEvent::Scroll {
                        dx: -x * 15.0,
                        dy: -y * 15.0,
                        v120_x: (-x * 120.0) as i32,
                        v120_y: (-y * 120.0) as i32,
                    },
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = self.placement.map_or(1.0, |p| p.scale);
                        InputEvent::Scroll { dx: (-p.x * s) as f32, dy: (-p.y * s) as f32, v120_x: 0, v120_y: 0 }
                    }
                };
                self.input(event);
            }
            _ => {}
        }
    }
}

/// From the connection to the decode thread, in order.
enum ToDecoder {
    /// A new epoch, with its size.
    Epoch(u16, Encoding, (u32, u32)),
    Frame(VideoFrame),
    Tiles(TilesPacket),
}

/// What a decoded frame or tiles datagram carries to the window.
enum Content {
    Picture(Picture),
    /// Tiles for a screen of `size`, each RGBX with rows `4 * w` bytes.
    Tiles { size: (u32, u32), tiles: Vec<(Rect, Vec<u8>)> },
}

fn decode_thread(
    hardware: bool,
    rx: mpsc::Receiver<ToDecoder>,
    proxy: EventLoopProxy<UserEvent>,
    client: Arc<OnceLock<Arc<Client>>>,
) {
    // The decoder for the current epoch's format, kept across epochs that
    // only change size.
    let mut decoder: Option<(Format, Decoder)> = None;
    let mut current = None;
    let mut tiles: Option<farsight_tiles::Decoder> = None;
    let mut screen = (0, 0);
    for msg in rx {
        let frame = match msg {
            ToDecoder::Epoch(epoch, encoding, size) => {
                tracing::info!(epoch, %encoding, ?size, "new epoch");
                screen = size;
                current = match encoding {
                    Encoding::Video(f) => Some(f),
                    Encoding::Tiles => None,
                };
                continue;
            }
            ToDecoder::Tiles(t) => {
                if tiles.is_none() {
                    match farsight_tiles::Decoder::new() {
                        Ok(d) => tiles = Some(d),
                        Err(err) => {
                            tracing::error!("{err:#}");
                            continue;
                        }
                    }
                }
                let mut out = Vec::new();
                let result = tiles.as_mut().unwrap().decode(&t.body, |rect, px| out.push((rect, px.to_vec())));
                if let Err(err) = result {
                    tracing::warn!(update = t.header.update, "{err:#}");
                }
                let decoded_us = client.get().map_or(0, |c| c.now_us());
                let d = Decoded {
                    content: Content::Tiles { size: screen, tiles: out },
                    capture_us: t.header.capture_us,
                    encode_us: t.header.encode_us,
                    complete_us: t.received_us,
                    decoded_us,
                    sample: t.header.index + 1 == t.header.count,
                };
                if proxy.send_event(UserEvent::Decoded(d)).is_err() {
                    return;
                }
                continue;
            }
            ToDecoder::Frame(f) => f,
        };
        let Some(format) = current else { continue };
        if decoder.as_ref().is_none_or(|(f, _)| *f != format) {
            // Close the old one first: one VA context at a time.
            decoder = None;
            match Decoder::new(format, hardware).or_else(|err| {
                tracing::warn!("{err:#}; trying software");
                Decoder::new(format, false)
            }) {
                Ok(d) => decoder = Some((format, d)),
                Err(err) => {
                    tracing::warn!("{err:#}");
                    if let Some(c) = client.get() {
                        c.decoder_failed(format);
                    }
                    // Nothing more to do until the next epoch.
                    current = None;
                    continue;
                }
            }
        }
        let (_, d) = decoder.as_mut().unwrap();
        match d.decode(&frame.data) {
            Ok(Some(picture)) => {
                let decoded_us = client.get().map_or(0, |c| c.now_us());
                let d = Decoded {
                    content: Content::Picture(picture),
                    capture_us: frame.header.capture_us,
                    encode_us: frame.header.encode_us,
                    complete_us: frame.complete_us,
                    decoded_us,
                    sample: true,
                };
                if proxy.send_event(UserEvent::Decoded(d)).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(frame = frame.header.frame, "{err:#}");
                if let Some(c) = client.get() {
                    c.request_keyframe();
                }
            }
        }
    }
}
