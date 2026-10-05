# farsight — design

Status: research and plan, 2026-10-05. Nothing below is implemented yet.

farsight is a low-latency remote desktop for headless Linux servers. Its
requirements:

1. **UDP-based.** Packet loss is tolerated in return for minimal delay, as in
   video streaming. This includes input sent to the server.
2. **Hardware video encoding first.**
3. **Codec negotiation.** Client and server agree on the best format both
   support. Clients decode in hardware where they can.
4. **Headless Wayland server.** farsight handles every part of Wayland,
   including creating and managing sessions.
5. **Resizing and HiDPI scaling first.** The remote desktop always matches
   the client window's size and scale.
6. **Clients:** desktop and Android. The Android app must work well with
   touch alone, like RealVNC Viewer.

## Decision: our own compositor

The server is a headless Wayland compositor written with
[Smithay](https://github.com/Smithay/smithay). It is not a capture tool
attached to someone else's compositor, the way wayvnc is. Owning the compositor
gives us:

- **Exact damage and frame timing.** We render only on damage, render
  straight into the encoder's input buffer, and pace app frames to the
  encoder instead of a fake vsync.
- **Resizing and scaling built in.** The client's window size and scale
  become the output's mode and scale in one atomic step.
- **Direct input injection** into `wl_seat`, with no virtual-keyboard
  protocols, libei or portals.
- **Control of the session lifecycle.**

Both [Wolf](https://games-on-whales.github.io/wolf/stable/dev/wayland.html)
(Games on Whales) and [wado](https://github.com/sandptel/wado) have shown that
a Smithay headless compositor can drive a zero-copy dmabuf → VA-API pipeline.

Later, a **mirror backend** can serve existing compositors (GNOME, KDE, sway)
using `ext-image-copy-capture`, the portal with PipeWire, and libei. It would
share the encoder and transport. Not in v1.

## Prior art

| Source | What we take |
|---|---|
| Sunshine/Moonlight | Frames split into packets with Reed-Solomon FEC (parity = data × fec%); intra-refresh and post-invalidation P-frames; per-frame FEC status from the client; reference frame invalidation instead of IDR |
| Parsec | Its own protocol on UDP (BUD); proof that a tuned protocol beats general-purpose ones |
| RDP 10 | AVC444: 4:4:4 text clarity from 4:2:0 hardware codecs, using a main stream plus an auxiliary chroma stream; a display control channel that carries the monitor layout and `DesktopScaleFactor` |
| gnome-remote-desktop | A headless session split into a system dispatcher and per-session daemons, with the connection handed over between them |
| wado, Wolf, Selkies pixelflux | Zero-copy dmabuf → VA-API/NVENC with on-GPU RGB→NV12; a fallback ladder (zero-copy → readback → software); input on a channel that never queues behind video |

## Architecture

```
                 ┌─────────────── server host ─────────────────────────────┐
 client ──QUIC──▶│ farsightd (root, systemd, :7740)                        │
   │  (auth)     │   auth → PAM/logind session → spawn/locate session      │
   │             │   returns {session addr, one-time ticket}               │
   │             │                                                         │
   └──QUIC──────▶│ farsight-session (as the user, one per session)         │
     (media,     │  ┌ Smithay compositor (GLES/Vulkan on render node)      │
      input,     │  │   xdg-shell, layer-shell, fractional-scale, viewporter│
      control)   │  │   linux-dmabuf, presentation-time, Xwayland, IME…    │
                 │  ├ Encoder: VA-API │ NVENC │ Vulkan Video │ software    │
                 │  ├ Transport: quinn + datagrams + FEC + congestion ctrl │
                 │  ├ Audio: PipeWire null sink → Opus                     │
                 │  └ Input → wl_seat directly                             │
                 └─────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Role |
|---|---|
| `farsight-proto` | Wire types: packet headers, control messages, negotiation. No I/O. |
| `farsight-net` | QUIC transport, packetization, FEC, congestion control. Shared by both sides. |
| `farsight-server` | `farsightd` (gatekeeper) and `farsight-session` (compositor, encoder). Linux only. |
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
- **Not WebRTC:** its jitter buffer and pacing are tuned for video calls and
  add latency we can't remove, and libwebrtc is heavy. **Not raw UDP:** we
  would have to rebuild encryption, migration and NAT handling.

## 2. Video pipeline

**Server, per frame:**

1. Damage arrives, or a frame callback is due. Composite into a GBM dmabuf.
2. Convert RGB→NV12 (or 4:4:4) in a shader during the same pass, so no VPP
   step is needed.
3. Hand the dmabuf to the encoder with zero copies:
   - **VA-API** (Intel/AMD): DRM-PRIME import.
   - **NVENC**: import the dmabuf through CUDA external memory and register
     it with NVENC in place. Re-import must be able to fail and be retried:
     Sunshine
     [hit `CUDA_ERROR_NOT_SUPPORTED`](https://github.com/LizardByte/Sunshine/issues/5613)
     after suspend.
   - **Vulkan Video encode**: RADV and ANV now have H.264, H.265 and AV1.
     This could become the single cross-vendor path later.
   - **Software fallback**: x264 or SVT-AV1 in low-delay mode.
4. Encoder settings:
   - no B-frames, an endless GOP, and VBV of about one frame;
   - slices, so the client can decode before the whole frame arrives;
   - LTR/RFI for loss recovery. Intra-refresh is the fallback and a full
     IDR the last resort.
5. **Idle refinement:** when damage stops, send one or two frames at a much
   lower QP so static text becomes sharp.
6. **Text clarity, in order of preference:** HEVC RExt 4:4:4 where both
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
- **The cursor is drawn on the client.** The server sends cursor images
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
- **The server applies size and scale in one atomic step:**
  - mode = physical pixels;
  - scale = the client's scale;
  - logical size = pixels ÷ scale.

  It then sends `wl_output`/`xdg_output` events and `preferred_scale` to
  every surface, so apps redraw sharply at the new scale. The client
  receives exactly its own pixel count and draws it 1:1.
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
- **Xwayland:** X apps blur under fractional scaling. Offer
  xwayland-satellite, or render X at scale 1 and let X apps scale
  themselves.
- **Multi-monitor (later):** one client window per output, each with its
  own encoder stream.

## 6. Session management

- **`farsightd`**: a small root daemon, socket-activated by systemd.
  - **Authentication:**
    - SSH-style Ed25519 user keys (`~/.config/farsight/authorized_keys`),
      with an optional PAM password or OTP;
    - the client pins the server certificate on first use (TOFU).
  - Calls `pam_open_session`, which runs `pam_systemd`, so logind
    registers a **seatless session** (`Class=user`, `Type=wayland`). That
    gives `XDG_RUNTIME_DIR`, the user's systemd manager and the D-Bus
    session bus.
  - Starts `farsight-session` as a user unit.
  - **GPU access:** logind grants device ACLs only to sessions with a seat.
    The gatekeeper opens `/dev/dri/renderD*` and passes the fd over
    `SCM_RIGHTS`, instead of requiring the `render` group.
  - **Handover:** the gatekeeper returns the session's address and a
    one-time ticket, and the client connects directly (0-RTT resumption).
    The gatekeeper is never in the media path.
- **`farsight-session`** runs as the user:
  - Starts PipeWire with its own null sink.
  - Starts `xdg-desktop-portal` with a small backend of our own.
  - Starts Xwayland.
  - Runs the user's autostart programs. Layer-shell lets bars and launchers
    such as waybar and fuzzel work unchanged.
- **Lifecycle:**
  - Sessions outlive disconnects (like tmux) until an idle timeout.
  - On reconnect, codec and layout are negotiated again.
  - A second client can take over the session or share it read-only.
- **Window manager:** keep v1 small: floating windows with
  maximise/fullscreen, plus a single-app kiosk mode. This is the largest
  scope risk.

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
  - Because we own the compositor, this mode can also forward real
    `wl_touch` contacts, so touch-aware apps get genuine multi-touch.

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
  commit (`text-input-v3` on the server); key events are used where the
  keyboard produces them.
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

- **Audio:** Opus with 5–10 ms frames and in-band FEC, sent as datagrams.
  Microphone redirection comes later.
- **Clipboard:** MIME-typed and fetched lazily, over a stream.
- **Latency telemetry:** a clock-offset exchange plus capture timestamps.
  The client reports present time, giving a measured glass-to-glass latency
  per stage. Target: under 16 ms on a LAN at 60 Hz.

## Milestones

| # | Goal | Done when |
|---|---|---|
| M0 | Spike: Smithay headless → dmabuf → VA-API H.264 → file | foot and Firefox render; encode latency per frame measured |
| M1 | End-to-end on the Linux desktop: quinn datagrams, packetizer, VA-API decode, present; input with repetition and snapshots; client-side cursor | Usable over LAN; latency measured |
| M2 | Resize/scale (`SetLayout`, epochs, fractional scale), negotiation, NVENC, HEVC/AV1, 4:4:4 and idle refinement | Drag-resize and a move to a different-DPI monitor both stay sharp |
| M3 | Sessions: gatekeeper, keys/PAM, seatless logind session, render-fd passing, persistence | Connect → login → session; reconnect resumes |
| M4 | Loss resilience: custom congestion control, adaptive FEC, RFI/LTR, NACK on LAN; `tc netem` test matrix | No stuck keys and no artifact spreading at 5% loss |
| M5 | Android client: MediaCodec low-latency, touch modes, viewport, extra keys, IME, audio, clipboard | Daily-usable from a phone or tablet |
| M6 | Mirror backend for GNOME/KDE/sway; multi-monitor; WebTransport browser client | Optional |

## Risks

1. **The window manager** expands scope. Keep it small and lean on
   layer-shell.
2. **NVIDIA zero-copy is fragile** (modifiers, CUDA import). Keep the
   readback fallback.
3. **Quinn's datagram congestion control** needs our own controller.
   Prototype it in M1.
4. **Android MediaCodec latency varies by vendor.** Measure on real devices
   early.
5. **HEVC/H.264 patent licensing** matters if this ships commercially. AV1
   avoids it.

## Sources

- [wado](https://github.com/sandptel/wado)
- [Wolf headless Wayland](https://games-on-whales.github.io/wolf/stable/dev/wayland.html)
- [Selkies pixelflux](https://github.com/selkies-project/pixelflux)
- [Sunshine UDP media streaming](https://deepwiki.com/qiin2333/foundation-sunshine/7.3-udp-media-streaming)
- [Sunshine NVENC dmabuf re-import issue](https://github.com/LizardByte/Sunshine/issues/5613)
- [GNOME headless remote sessions, part 2](https://www.suse.com/c/headless-remote-sessions-in-gnome-part-2/)
- [RDP 10 AVC444](https://techcommunity.microsoft.com/blog/microsoft-security-blog/remote-desktop-protocol-rdp-10-avch-264-improvements-in-windows-10-and-windows-s/249588)
- [ext-image-copy-capture merged](https://www.phoronix.com/news/Wayland-Merges-Screen-Capture)
- [RADV AV1 encode](https://www.phoronix.com/news/RADV-Merges-AV1-Encode)
- [ANV AV1 Vulkan encode](https://www.phoronix.com/news/Intel-DG2-Vulkan-Video-AV1)
- [SCReAM v2 draft](https://www.ietf.org/archive/id/draft-johansson-ccwg-rfc8298bis-screamv2-04.html)
- [QUIC streams vs datagrams](https://www.ni-sp.com/background-on-quic-streams-and-quic-datagrams/)
- [libei](https://libinput.pages.freedesktop.org/libei/api/index.html)
