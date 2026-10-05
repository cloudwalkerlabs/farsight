# farsight — design

Status: plan, 2026-10-05. The M0 spike is done and option B holds; see
[m0-results.md](m0-results.md). Nothing past M0 is implemented yet.

farsight is a low-latency remote desktop for headless Linux servers. Its
requirements:

1. **UDP-based.** Packet loss is tolerated in return for minimal delay, as in
   video streaming. This includes input sent to the server.
2. **Hardware video encoding first.**
3. **Codec negotiation.** Client and server agree on the best format both
   support. Clients decode in hardware where they can.
4. **Headless Wayland server.** farsight handles every part of Wayland,
   including bringing up the session itself. One `farsight-server` process
   is one long-running, isolated session that clients can reconnect to.
5. **Resizing and HiDPI scaling first.** The remote desktop always matches
   the client window's size and scale.
6. **Clients:** desktop and Android. The Android app must work well with
   touch alone, like RealVNC Viewer.

## Decision: a host compositor, with the desktop nested inside it

`farsight-server` is a **small headless Wayland compositor** written with
[Smithay](https://github.com/Smithay/smithay). Its main client is a
**real desktop compositor running nested**: labwc (for example
`labwc --session xfce4-session`), sway, or anything else with a Wayland
backend. The nested compositor draws its whole desktop into one fullscreen
surface on the host. The host encodes that surface and feeds input back
through its seat.

```
farsight-server ── runs ──▶ labwc --session xfce4-session
  (host compositor:           draws its desktop into one surface on the host
   one fullscreen surface,
   encode, input, transport)
```

Nesting is a normal way to run a compositor; its developers use it every
day. gamescope uses the same structure to stream games and desktops.

**What the host gives us** that a capture tool attached to someone else's
compositor (as wayvnc is) cannot:

- **Frames with no capture step.** The nested compositor's output buffer
  arrives as a dmabuf on commit, with exact damage, and goes straight to the
  encoder.
- **Frame pacing.** The host sends the frame callbacks, so the nested
  compositor draws exactly when the encoder is ready.
- **A cursor drawn on the client.** The nested compositor sets its cursor as
  a separate surface on the host's seat.
- **Real input:** keyboard with our keymap, pointer, and real `wl_touch`.
  wayvnc relies on virtual keyboard/pointer protocols, and wlroots has no
  virtual touch protocol.
- **Resizing** by resizing the nested compositor's window. Scale needs more
  work (§5).
- **Control of the session lifecycle**, without a display manager.

**What it costs** compared with being the desktop compositor ourselves:

- **One more process boundary.**
  - The nested compositor composites, then we encode, so colour conversion
    is a separate GPU pass.
  - If the nested compositor picks a buffer format the encoder can't
    import, we pay a GPU blit.
  - Its frame scheduling sits between the apps and us.
- **The host is also a Wayland client *of* the nested compositor:**
  - clipboard, through `ext-data-control`;
  - IME and soft-keyboard text, through `input-method-v2`;
  - output scale, through `wlr-output-management`.

  wlroots' Wayland backend shares none of these with its host.
- **Compositor support depends on its Wayland backend.**
  - First-class and tested: **labwc** (including labwc + XFCE) and **sway**.
  - Best effort: Hyprland, niri, KWin, Weston.
  - Not supported: GNOME, because mutter's nested mode is for development
    only.

**Kiosk mode:** the host can also run a single app directly, with no nested
compositor. This is also the first step towards option A below, so choosing
B now doesn't close off A.

### Alternatives considered

**A: our own desktop compositor.** The server is a complete compositor with
its own window manager, and apps connect to it directly.

- **Pros:**
  - The lowest possible latency: app surfaces are composited straight into
    the encoder's buffer, with colour conversion in the same pass.
  - Resize and scale apply in one atomic step.
  - IME, clipboard and touch are direct.
  - A single process.
- **Cons:** we build the whole desktop compositor except the hardware
  parts:
  - 25–30 protocols with our own behaviour behind each: layer-shell,
    foreign-toplevel, session-lock, data-control, input-method,
    image-copy-capture, …;
  - a window manager: placement, focus, move/resize grabs, popups, dialogs,
    workspaces, keybindings, rules;
  - server-side decorations;
  - an Xwayland window manager;
  - configuration;
  - a portal backend.

  Headless does remove DRM/KMS, libinput, hotplug, VT switching and
  multi-GPU. The desktop would stay minimal for a long time.
- **Viable** if B's measured latency turns out to matter, or if we want a
  WM designed around touch. The host compositor grows into A rather than
  being thrown away.

**A′: fork a Smithay desktop compositor** (niri, or cosmic-comp), replacing
its DRM and libinput backends with a farsight backend.

- **Pros:** A's latency, with a real desktop on day one.
- **Cons:**
  - We maintain a fork of a fast-moving project.
  - We inherit its WM style: niri is a scrolling tiler, and cosmic-comp is
    tied to COSMIC.
  - labwc + XFCE is no longer an option.
- **Viable**, and the cheaper fallback if A is ever needed.

**Capture an existing compositor (the wayvnc model).**

- **How:** attach to a running compositor as a client, using
  `ext-image-copy-capture` or the portal with PipeWire, plus virtual input
  or libei.
- **Pros:** it can share a physical desktop and works with GNOME.
- **Cons:** no control over frame timing, no real touch, and an extra
  capture step.
- **Status:** kept only as a possible later "mirror backend" (M6).

Both [Wolf](https://games-on-whales.github.io/wolf/stable/dev/wayland.html)
(Games on Whales) and [wado](https://github.com/sandptel/wado) have shown that
a Smithay headless compositor can drive a zero-copy dmabuf → VA-API pipeline.

## Prior art

| Source | What we take |
|---|---|
| Sunshine/Moonlight | Frames split into packets with Reed-Solomon FEC (parity = data × fec%); intra-refresh and post-invalidation P-frames; per-frame FEC status from the client; reference frame invalidation instead of IDR |
| Parsec | Its own protocol on UDP (BUD); proof that a tuned protocol beats general-purpose ones |
| RDP 10 | AVC444: 4:4:4 text clarity from 4:2:0 hardware codecs, using a main stream plus an auxiliary chroma stream; a display control channel that carries the monitor layout and `DesktopScaleFactor` |
| gamescope | A host compositor with a whole compositor or game nested inside it, streaming the result |
| wado, Wolf, Selkies pixelflux | Zero-copy dmabuf → VA-API/NVENC with on-GPU RGB→NV12; a fallback ladder (zero-copy → readback → software); input on a channel that never queues behind video |

## Architecture

```
                 ┌──────── farsight-server --port 7740 (as the user) ──────┐
 client ──QUIC──▶│  ┌ auth (client keys), session state                    │
  (media,        │  ├ private runtime dir, D-Bus session bus, PipeWire     │
   input,        │  ├ host compositor (Smithay, render node)               │
   control)      │  │   xdg-shell, linux-dmabuf (+feedback), seat, cursor, │
                 │  │   fractional-scale, viewporter, presentation-time    │
                 │  ├ client of the nested compositor: data-control,       │
                 │  │   input-method, output-management                    │
                 │  ├ Encoder: VA-API │ NVENC │ Vulkan Video │ software    │
                 │  ├ Transport: quinn + datagrams + FEC + congestion ctrl │
                 │  └ Audio: PipeWire sink/source streams ↔ Opus           │
                 └───────────────▲──────────────────┬──────────────────────┘
                     one surface │                  │ wl_seat input
                 ┌───────────────┴──────────────────▼──────────────────────┐
                 │ nested desktop: labwc --session xfce4-session (child)   │
                 │   apps, Xwayland, panels, portals connect to it         │
                 └─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Role |
|---|---|
| `farsight-proto` | Wire types: packet headers, control messages, negotiation. No I/O. |
| `farsight-net` | QUIC transport, packetization, FEC, congestion control. Shared by both sides. |
| `farsight-server` | The server: session environment, host compositor, nested desktop, encoder. Linux only. |
| `farsight-client` | Platform-independent client core: connection, negotiation, decode pipeline, input state sync, layout. |
| `farsight-desktop` | Desktop client. |
| `farsight-android` | Android bindings (uniffi, plus JNI for hot paths). |
| `farsight-uniffi-bindgen` | uniffi's Kotlin generator, run by the Gradle build. |

## 1. Transport

- **QUIC ([quinn](https://github.com/quinn-rs/quinn)) with RFC 9221
  unreliable datagrams** for video, audio, input and cursor position.
  **Streams** carry control, negotiation, clipboard, cursor images and
  files.
- What QUIC gives us for free:
  - TLS 1.3;
  - **connection migration** (a phone moving from Wi-Fi to LTE keeps its
    session);
  - path MTU discovery (DPLPMTUD) and keepalives;
  - one UDP port;
  - a later browser client through WebTransport.
- **Catch:** QUIC datagrams are congestion-controlled, and quinn's built-in
  controllers are built for bulk transfer. We plug in our own `Controller`:
  a delay-based media controller modelled on
  [SCReAM v2](https://www.ietf.org/archive/id/draft-johansson-ccwg-rfc8298bis-screamv2-04.html)
  (which supports L4S) or on WebRTC's GCC. Its bandwidth estimate drives the
  encoder's target bitrate directly. Prototype it early, because it shapes
  the whole latency profile.
- **Datagram priority.** quinn keeps every outgoing datagram in one FIFO,
  so input or audio queued behind a keyframe waits for all of it to be
  sent: about 120 ms for 300 KB at 20 Mbps. `farsight-net` puts a
  scheduler in front of it:
  - strict priority: input and ping > audio > video;
  - quinn's own buffer holds only about one pacing interval, and the
    rest waits in per-class queues, where only video may grow or drop;
  - video, keyframes included, is paced at the controller's rate, so it
    never fills the congestion window in one burst;
  - audio's ~300 kbps comes off the estimate before the video bitrate is
    set.
- **Plaintext mode** (`--no-tls`), for networks that already encrypt and
  authenticate, such as Tailscale or WireGuard. See below.
- **Not WebRTC:** its jitter buffer and pacing are tuned for video calls and
  add latency we can't remove, and libwebrtc is heavy. **Not raw UDP:** we
  would have to rebuild encryption, migration and NAT handling.

### Plaintext mode

When the server is only reachable over a tailnet or a WireGuard tunnel, the
network already encrypts every packet and authenticates every peer, and
encrypting again in QUIC is redundant. `--no-tls` turns QUIC's encryption
off.

- **Still QUIC.** Streams, retransmission, datagrams, migration and
  congestion control all stay. Only the crypto layer changes: quinn's
  crypto is pluggable (`quinn::crypto::{ClientConfig, ServerConfig,
  Session}`), and plaintext mode plugs in a **null session**:
  - the handshake carries only the transport parameters, one message each
    way;
  - packet keys and header protection do nothing, and the packet tag is
    empty, so there is no integrity check beyond the UDP checksum.
- **Its own ALPN-like identity, `farsight-plain/0`,** carried in the null
  handshake. A TLS endpoint and a plaintext endpoint fail to connect
  instead of misreading each other.
- **Opt-in on both ends, never a fallback.**
  - The server serves either TLS or plaintext on a port, not both.
  - The client stores the mode per server in its address book and never
    retries a failed TLS connection in plaintext, so no one can downgrade
    it.
- **Guard rails on the server:**
  - `--no-tls` requires an explicit `--listen <addr>`: no wildcard bind.
  - It warns unless that address is loopback, in Tailscale's range
    (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), or on an interface named
    `tailscale*` or `wg*`.
- **Authentication:** client keys (§6) still apply, so other users on the
  same tailnet can't connect. Without TLS there is no channel to bind the
  key to, so the client signs a server nonce in the null handshake instead.
  That proves who connected but protects nothing after it: the network
  must stop on-path attackers, which Tailscale and WireGuard do.
- **What it saves is small.** AES-GCM with AES-NI, or the ARMv8 crypto
  extensions on phones, costs well under 1% of a core at 100 Mbit/s, and
  adds microseconds per packet. The main gain is one less layer to debug
  (packets are readable in Wireshark) and less CPU on weak clients.
  Measure it before recommending it.
- **Audio and the microphone travel unencrypted too.** The warning says
  so: the network must be trusted with what is heard and said, not only
  with the screen.

## 2. Video pipeline

**Server, per frame:**

1. The nested compositor commits its output buffer (a dmabuf) with damage.
   Through `linux-dmabuf` feedback, the host offers only the formats and
   modifiers the encoder can import, so the buffer can be used as is. If the
   buffer can't be used directly, a GPU blit is the fallback.
2. Convert RGB→NV12 (or 4:4:4) in one shader pass into an encoder surface,
   so no VPP step is needed. In kiosk mode, or under option A, this happens
   in the same pass as compositing.
3. Release the nested buffer and send the frame callback **as soon as the
   conversion pass has read the buffer**. Don't wait for the encode to
   finish, so the nested compositor can start its next frame at once.
4. Hand the encoder surface to the encoder with zero copies:
   - **VA-API** (Intel/AMD): DRM-PRIME import.
   - **NVENC**: import the dmabuf through CUDA external memory and register
     it with NVENC in place. Re-import must be able to fail and be retried:
     Sunshine
     [hit `CUDA_ERROR_NOT_SUPPORTED`](https://github.com/LizardByte/Sunshine/issues/5613)
     after suspend.
   - **Vulkan Video encode**: RADV and ANV now have H.264, H.265 and AV1.
     This could become the single cross-vendor path later.
   - **Software fallback**: x264 or SVT-AV1 in low-delay mode.
5. Encoder settings:
   - no B-frames, an endless GOP, and VBV of about one frame;
   - slices, so the client can decode before the whole frame arrives;
   - LTR/RFI for loss recovery. Intra-refresh is the fallback and a full
     IDR the last resort.
6. **Idle refinement:** when damage stops, send one or two frames at a much
   lower QP so static text becomes sharp.
7. **Text clarity, in order of preference:** HEVC RExt 4:4:4 where both
   ends support it; then an AVC444-style dual stream; then 4:2:0 with idle
   refinement.

**Packetization and loss:**

- Split each frame into fragments that fit the MTU. Each header carries:
  - frame number and type;
  - fragment index and count;
  - FEC block;
  - capture timestamp and encode duration;
  - the video epoch (§5).
- Apply **adaptive Reed-Solomon FEC** to each frame, sized from the measured
  loss. Keyframes get extra parity. Interleave fragments when loss is bursty.
- **When RTT is under one frame interval** (LAN), selectively NACK and
  retransmit. **Otherwise**, use FEC plus RFI: the client reports "frame N
  lost, last good M" and the encoder references M.
- **The client never decodes a damaged frame.** It drops the frame, sends
  an RFI and keeps showing the last good frame.
- **No jitter buffer.** Decode as soon as a frame is complete, present
  immediately, and drop frames that are already superseded.

## 3. Codec negotiation

- At startup the server probes its hardware: `vaQueryConfigEntrypoints`,
  NVENC caps, and Vulkan video profile queries.
- When it connects, the client sends one entry per decoder it has
  (`farsight_proto::codec::DecoderCaps`), each covering:
  - codec, profile and level;
  - chroma format and bit depth;
  - maximum size;
  - hardware or software;
  - whether it supports slices and partial decode, and LTR/RFI;
  - a measured decode latency (the client runs a quick test decode).
- The server intersects the two lists and ranks the matches:
  1. Hardware on both ends.
  2. The mode the user asked for: "text" mode prefers 4:4:4, "motion" mode
     prefers efficiency.
  3. Codec efficiency: AV1 > HEVC > H.264.
  4. Latency.
- The server returns its choice together with the fallback order.
  **Negotiation can run again mid-session**: when a resize exceeds the codec
  level, the user switches mode, or a decoder fails.
- Client-side decoders:

| Platform | Decoder | Zero-copy display path |
|---|---|---|
| Linux | VA-API or Vulkan Video | dmabuf → EGL/Vulkan |
| Android | MediaCodec with `KEY_LOW_LATENCY` | output straight to a `Surface` |
| macOS | VideoToolbox | |
| Windows | D3D11VA | |

## 4. Input over UDP

A lost video frame is acceptable. A lost key-up is not, because it leaves a
key stuck down. So input travels as unreliable datagrams but is designed as
**state synchronisation**:

- Every input datagram carries a sequence number, **the new event, plus the
  last N events repeated**. A single lost packet costs nothing.
- **A state snapshot goes out every ~50–100 ms, and in every packet while a
  key is held.** It holds the pressed keys and buttons, pointer position,
  and modifier/lock state. The server compares it with its seat state and
  generates corrections, so a stuck key fixes itself within one snapshot
  interval.
- **Absolute pointer motion:** the latest position wins and stale updates
  are dropped.
- **Relative motion and scroll:** deltas accumulate per sequence number and
  are repeated in later packets, so nothing is lost or counted twice.
- **Touch:** each contact is tracked by id and position and synced the same
  way (§7).
- **Text commits, IME and clipboard** go over a reliable stream.
- Input is sent immediately with no batching, limited to about 1 kHz.
- **The server injects input** through the host's `wl_seat` into the nested
  compositor, which routes it to apps as it would input from real devices.
  Text commits go through the host's `input-method-v2` connection to the
  nested compositor.
- **The cursor is drawn on the client.** The nested compositor sets its
  cursor as a surface on the host's seat. The server sends cursor images
  over a stream (cached by hash) and shape and visibility changes as
  datagrams. The client draws the cursor locally, so pointer movement has
  zero perceived latency.

## 5. Resizing and HiDPI scaling

The client's window is the source of truth for the output
(`farsight_proto::layout::Layout`).

- **The client sends `SetLayout`:** `{width_px, height_px, scale_120,
  refresh}` for each monitor. Each platform supplies the scale differently:

| Platform | Scale source |
|---|---|
| Wayland | `wp_fractional_scale_v1` |
| Android | `DisplayMetrics.density` |
| macOS | `backingScaleFactor` |
| Windows | `GetDpiForWindow` |

  The client resends it on resize, on a move to a monitor with a different
  scale, and on an OS scale change.
- **The server applies size and scale to the nested compositor:**
  - **Size:** the host reconfigures the nested compositor's fullscreen
    window to the new physical size, and the nested compositor resizes its
    output to match.
  - **Scale:** the host sets the output's scale and sends `preferred_scale`
    to the nested window. If the nested compositor doesn't take its output
    scale from that, the host also sets it through `wlr-output-management`
    as a client of the nested compositor (which is what wayvnc does).
  - The nested compositor then tells its apps the new scale, and they
    redraw sharply. The client receives exactly its own pixel count and
    draws it 1:1.
  - **Measured in M0:** the change is not one step. Frames at the new size
    with old content reach the host, so the host holds the last good frame
    until the nested compositor confirms the new output state and a frame
    arrives after it. labwc also ignores the host's `preferred_scale`, so
    with labwc scale always goes through `wlr-output-management`
    ([m0-results.md](m0-results.md#fractional-scale)).

  Under option A (or in kiosk mode) the host applies both in one atomic
  step itself.
- **The server can clamp the request**, for example to a codec level's
  limit. It replies with the layout actually in effect, and the client
  scales only in that case.
- **Odd sizes:** the encode surface is padded to the codec's alignment, and
  the codec's crop/conformance window is set.
- **Each resize starts a new video epoch:**
  - NVENC: `nvEncReconfigureEncoder`.
  - VA-API: recreate the context.
  - Either way, the epoch starts on a keyframe or intra-refresh.
  - Allocating encode surfaces at the maximum size avoids reallocating.
- **Live drag-resize:** the client sends at most one `SetLayout` every
  ~50 ms and stretches the last frame until a frame from the new epoch
  arrives.
- **Xwayland:** the nested compositor runs Xwayland, so X app scaling is
  its policy. X apps blur under fractional scaling in labwc as they do
  anywhere else.
- **Multi-monitor (later):** one client window per output, each with its
  own encoder stream. The nested compositor gets one host window per output
  (the wlroots Wayland backend supports several).

## 6. The session

One `farsight-server` process **is** one session. There is no gatekeeper,
no PAM, no logind session and no display manager. Run it by hand, or as a
plain system service (`dist/farsight-server@.service`, one instance per
port, with `User=` set). It doesn't need user systemd or lingering.

```
farsight-server [--port 7740] [-- <desktop command>]    # default: labwc
farsight-server --port 7741 -- labwc --session xfce4-session
farsight-server --no-tls --listen 100.101.102.103    # tailnet only (§1)
```

**Startup** builds an isolated environment and starts the session at once,
so it is already running before the first client connects:

1. **Runtime directory**, mode 0700, holding every socket. It is the first
   of these that exists:
   - systemd's `$RUNTIME_DIRECTORY`;
   - `$XDG_RUNTIME_DIR/farsight-<port>`;
   - a fresh private directory under `/tmp`.

   Children get it as their `XDG_RUNTIME_DIR`.
2. **A private D-Bus session bus**: `dbus-daemon --session` (or
   `dbus-broker-launch`) listening in that directory. Children get it as
   `DBUS_SESSION_BUS_ADDRESS`. Apps in the session never see another bus.
3. **The host compositor**: its Wayland socket goes in that directory and
   is given only to the nested compositor.
4. **Session services**, launched as child processes on the private bus:
   PipeWire, WirePlumber and pipewire-pulse, with no access to the
   server's sound hardware (§8). The server creates the session's only
   sink and source itself.
5. **The desktop**: the desktop command, `labwc` by default, started with
   the host's `WAYLAND_DISPLAY`.
   - The nested compositor creates its own Wayland socket for apps, along
     with Xwayland and its usual autostart. labwc's `--session` starts
     xfce4-session, panels, portals (`xdg-desktop-portal-wlr` for wlroots
     desktops) and so on, exactly as on a physical machine.
   - Once its socket is up, the host connects to it as a client, for
     clipboard, IME and output scale (§5).
   - **If the desktop exits or crashes,** the host and the client connection
     survive. The host restarts the desktop, or ends the session, depending
     on its exit status and configuration.
6. **Kiosk mode** (`--app <command>`): there is no nested compositor. The
   app connects straight to the host, which shows it fullscreen.

**Isolation:**
- The environment comes from the steps above, not from whatever started
  the server, so two instances on different ports never share a bus or a
  display.
- Children are in their own process group and are cleaned up when the
  server exits. Under systemd the unit's cgroup guarantees this.
- **Sound hardware is out of reach** (§8): WirePlumber's ALSA, Bluetooth
  and camera monitors are off, and the unit sets
  `InaccessiblePaths=-/dev/snd -/run/pulse -/run/user/%U`.
- Socket paths are limited to 108 bytes, and the longest is
  `<rundir>/pulse/native`. The server checks this at startup.

**GPU:** render nodes (`/dev/dri/renderD*`) are normally world-accessible,
so the session needs no seat or `video` group to render and encode. If a
host restricts them, add the user to the `render` group.

**Clients:**
- **Authentication:** SSH-style Ed25519 client keys
  (`~/.config/farsight/authorized_keys`). The client pins the server's
  certificate on first use (TOFU).
  In plaintext mode (§1) there is no certificate to pin; the client key
  is checked with a signed nonce instead, and the network provides the
  rest.
- **Reconnection:** the session outlives disconnects. A reconnecting client
  negotiates codec and layout again, and its layout is applied (§5).
- **More than one client:** a second client can either take over the
  session or join it view-only.

**Window manager:** none of our own. Window management, decorations,
panels and Xwayland belong to the nested desktop (see "Alternatives
considered" for building our own).

**Later, if ever:** a login-screen style front end that starts sessions for
any system user (as GDM's remote login does). It is deliberately out of
scope; it would sit in front of `farsight-server` without changing the
protocol.

## 7. Android client: touch-first, like RealVNC Viewer

The Android app must be fully usable with fingers alone. A keyboard and
mouse, or a desktop-mode display, are a bonus. RealVNC Viewer sets the bar.

**Input modes** (switchable per connection and from the toolbar):

- **Touchpad mode** (default for small screens):
  - The finger moves a cursor relatively, as on a laptop trackpad.
  - Tap = left click, two-finger tap = right click, tap-and-hold-then-drag
    = drag.
  - Two-finger drag = scroll; three-finger tap = middle click.
- **Direct touch mode:**
  - The pointer jumps to the finger, so a tap clicks at that spot.
  - Long press = right click.
  - Because the host is a real compositor, this mode can also forward real
    `wl_touch` contacts through the nested compositor, so touch-aware apps
    get genuine multi-touch. How well that works depends on the nested
    compositor's touch handling, which is basic in labwc.

**Viewport (local, never sent to the server):**

- Pinch to zoom and two-finger pan over the remote desktop, with smooth
  inertia.
- The viewport follows the cursor when it nears an edge.
- Double-tap with two fingers to switch between fit and 1:1.
- The decoded frame is kept at full resolution, and zooming is just GPU
  scaling of the last frame, so it costs nothing.
- With resize enabled (§5), "fit" means the server adopts the screen's
  size and scale. With resize off, the client scales the frame.

**Keyboard:**

- The soft keyboard is opened from the toolbar. Typed text goes through IME
  commit (the host's `input-method-v2` connection to the nested
  compositor, which passes it on to apps over `text-input-v3`). Key events
  are used where the keyboard produces them.
- An **extra keys bar** above the keyboard has sticky modifiers (Ctrl, Alt,
  Super, Shift), Esc, Tab, arrows, F-keys and a key-combo builder.
- Hardware keyboards are passed through with physical scancodes.

**Chrome:**

- A small, draggable, auto-hiding toolbar for: keyboard, input mode, extra
  keys, clipboard sync, zoom to fit/1:1, and disconnect.
- Immersive full screen, with `adjustResize` so the keyboard shrinks the
  viewport instead of covering the cursor.
- The Activity is never recreated: config changes are handled in place.
- An address book of saved servers with pinned keys, and a list of recent
  connections with thumbnails.

**Mouse and desktop mode:** a pointer device bypasses touch modes and is
sent directly, with relative pointer capture when the server asks for it.

## 8. Everything else

- **Audio** ([research](research/audio.md)):
  - **Isolation.** The session's PipeWire runs in its private runtime dir.
    WirePlumber runs a `farsight` profile with the hardware monitors off
    (`hardware.audio`, `hardware.bluetooth` and `hardware.video-capture`
    disabled), so the session has no hardware devices.
    - The environment sets `PULSE_SERVER` explicitly, so libpulse never
      falls back to a system instance or autospawns one.
    - It sets `JACK_NO_START_SERVER=1`.
    - pipewire-alsa is a dependency, so ALSA's `default` is PipeWire.
    - The audio daemons get their own `XDG_CONFIG_HOME` and a per-session
      `XDG_STATE_HOME`.
  - **Server side.** `farsight-server` is a PipeWire client with two
    streams: `farsight-speaker` (`Audio/Sink`) and `farsight-mic`
    (`Audio/Source`). They live as long as the session, and WirePlumber
    makes them the defaults. The graph runs at 48 kHz with a fixed
    quantum of 240, one 5 ms frame per cycle, on the same monotonic clock
    as video timestamps.
  - **Desktop audio:** Opus `RESTRICTED_LOWDELAY` (CELT only), 5 ms
    frames, stereo, 96–128 kbps.
    - **Loss:** each datagram repeats the two previous frames. Opus
      in-band FEC exists only in SILK mode, which needs frames of at least
      10 ms.
    - **Bad links:** fall back to 10 ms frames.
    - **Silence:** nothing is sent while the sink is idle.
  - **Client:** an adaptive jitter buffer of 5–20 ms, Opus PLC, and
    drift correction by adaptive resampling. Video is never held back
    for audio: audio trails it by 10–30 ms, well inside the 125 ms
    detection threshold for late audio.
  - **Microphone:** on demand.
    - When an app starts recording from `farsight-mic`, the server sends
      `MicDemand(true)`. The client opens its mic according to the user's
      setting (never, ask or always) and shows an indicator.
    - Opus `VOIP` mode, 10 ms frames, mono, with in-band FEC.
    - **Echo cancellation runs on the client:** AAudio's
      `VOICE_COMMUNICATION` preset on Android, and WebRTC's AEC3 on the
      desktop. It can be turned off for headphones.
  - **Wire:** datagram tags `TAG_AUDIO` and `TAG_MIC`, each carrying a
    sequence number, `capture_us` and the newest frames. On the control
    stream: `AudioCaps` in `Hello`, plus `AudioConfig`, `MicDemand` and
    `SetAudio`.
- **Clipboard:** MIME-typed and fetched lazily, over a stream. The server
  side reads and sets the nested desktop's clipboard through
  `ext-data-control`.
- **Latency telemetry:** a clock-offset exchange plus capture timestamps.
  The client reports present time, giving a measured glass-to-glass latency
  per stage. Target: under 16 ms on a LAN at 60 Hz.

## Milestones

| # | Goal | Done when |
|---|---|---|
| M0 | Spike: Smithay host compositor with labwc (+ XFCE) nested → dmabuf → VA-API H.264 → file | Desktop renders. Measured: app commit → encoder latency through labwc; whether labwc's buffers import with no blit; resize and fractional-scale behaviour; keymap pass-through. These numbers decide whether B holds or A/A′ is needed. **Done: B holds** ([results](m0-results.md)). |
| M1 | End-to-end on the Linux desktop: quinn datagrams, packetizer, datagram priority scheduler and video pacing (§1), VA-API decode, present; input with repetition and snapshots; client-side cursor | Usable over LAN; latency measured; input latency doesn't rise during keyframes. **Done over loopback** ([results](m1-results.md)): 10 ms from commit to the client's swap; a run between two machines is still to do. |
| M2 | Resize/scale (`SetLayout`, epochs, fractional scale), negotiation, NVENC, HEVC/AV1, 4:4:4 and idle refinement | Drag-resize and a move to a different-DPI monitor both stay sharp |
| M3 | Session: isolated runtime dir, private D-Bus, PipeWire, desktop supervision and restart, kiosk mode, clipboard/IME via the nested compositor, client keys, reconnect and takeover; plaintext mode (`--no-tls`). **Audio out:** isolated audio daemons, `farsight-speaker`, Opus with redundancy, desktop client playback with jitter buffer and drift correction | Runs as a system service; reconnect resumes the same session; a video in the session plays on the client while the server's speakers stay silent, even with the user in `audio`; audio latency measured |
| M4 | Loss resilience: custom congestion control, adaptive FEC, RFI/LTR, NACK on LAN; audio redundancy depth and 10 ms fallback; `tc netem` test matrix | No stuck keys, no artifact spreading and no audible audio gaps at 5% loss |
| M5 | Android client: MediaCodec low-latency, touch modes, viewport, extra keys, IME, audio (AAudio), clipboard. **Microphone** on both clients: `farsight-mic`, `MicDemand`, client capture with echo cancellation | Daily-usable from a phone or tablet; a call app in the session hears the client's mic without echo |
| M6 | Mirror backend for GNOME/KDE/sway; multi-monitor; WebTransport browser client | Optional |

## Risks

1. **Nested compositors' Wayland backends** are mostly used for
   development, and each behaves a little differently (scale, buffer
   formats, multiple outputs). labwc and sway are the tested targets;
   everything else is best effort. If the M0 numbers are bad, fall back to
   A′, then A.
2. **NVIDIA zero-copy is fragile** (modifiers, CUDA import). Keep the
   readback fallback.
3. **Quinn's datagram congestion control** needs our own controller.
   Prototype it in M1.
4. **Android MediaCodec latency varies by vendor.** Measure on real devices
   early.
5. **HEVC/H.264 patent licensing** matters if this ships commercially. AV1
   avoids it.
6. **Audio output latency varies by device**, Android especially (10–40 ms
   in the output buffer). Measure it alongside MediaCodec.

## Sources

- [wado](https://github.com/sandptel/wado)
- [Wolf headless Wayland](https://games-on-whales.github.io/wolf/stable/dev/wayland.html)
- [Selkies pixelflux](https://github.com/selkies-project/pixelflux)
- [Sunshine UDP media streaming](https://deepwiki.com/qiin2333/foundation-sunshine/7.3-udp-media-streaming)
- [Sunshine NVENC dmabuf re-import issue](https://github.com/LizardByte/Sunshine/issues/5613)
- [RDP 10 AVC444](https://techcommunity.microsoft.com/blog/microsoft-security-blog/remote-desktop-protocol-rdp-10-avch-264-improvements-in-windows-10-and-windows-s/249588)
- [ext-image-copy-capture merged](https://www.phoronix.com/news/Wayland-Merges-Screen-Capture)
- [RADV AV1 encode](https://www.phoronix.com/news/RADV-Merges-AV1-Encode)
- [ANV AV1 Vulkan encode](https://www.phoronix.com/news/Intel-DG2-Vulkan-Video-AV1)
- [SCReAM v2 draft](https://www.ietf.org/archive/id/draft-johansson-ccwg-rfc8298bis-screamv2-04.html)
- [QUIC streams vs datagrams](https://www.ni-sp.com/background-on-quic-streams-and-quic-datagrams/)
- [libei](https://libinput.pages.freedesktop.org/libei/api/index.html)
