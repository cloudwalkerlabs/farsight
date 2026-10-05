# M3 — results

Status: measured 2026-10-05 on master. M3 makes the server a session:
an isolated environment, supervision, clients that authenticate, come and
go and share it, plaintext mode, clipboard and text input, kiosk mode, and
audio out ([design.md, Milestones](design.md#milestones)).

**Verdict: done, over loopback on one machine, with two gaps.** The
session survives clients leaving, losing their network and taking over
from each other, and its desktop crashing. Audio reaches the client
13–20 ms after the session's sink captures it, and the machine's own sound
server never sees it. Not checked: a *system* service (no root here; a
transient user service with the same settings stands in) and a user in
the `audio` group (this user isn't).

## What M3 added

- **The session's environment** (§6, `session.rs`): a 0700 runtime
  directory (`$RUNTIME_DIRECTORY`, `$XDG_RUNTIME_DIR/farsight-<port>`, or
  under `/tmp`), checked for socket-path length; a private `dbus-daemon`;
  PipeWire, WirePlumber (profile `farsight`, every hardware monitor off)
  and pipewire-pulse with their own configuration and state. Children get
  an environment built from scratch, lead their own process groups, die
  with the server (`PR_SET_PDEATHSIG`) and are stopped on SIGTERM, SIGINT
  or SIGHUP. The host's Wayland socket lives in the runtime directory.
- **Desktop supervision:** a crash restarts the desktop after 0.5 s,
  doubling; five crashes in a minute, or a clean exit, end the session
  (`--restart on-failure|always|never`). Clients stay connected through a
  restart and get the new desktop in a new epoch.
- **Client keys** (§6): Ed25519, `client_key` in the client's config
  directory, OpenSSH's `ssh-ed25519` line in the server's
  `authorized_keys` (read on every connection; `farsight-desktop
  --print-key` prints it). The client signs keying material exported from
  the connection, so a signature is bound to it. **TOFU:** the client pins
  the server's certificate in `known_hosts` and refuses a changed one
  before sending anything.
- **Plaintext mode** (`--no-tls --listen ADDR`, §1): a null crypto layer
  for quinn. Its handshake carries the transport parameters, the identity
  `farsight-plain/0` and a random value from each side, which the
  exported keying material derives from, so client keys work unchanged.
  It runs on a QUIC version of its own (`0x46535000`): a TLS client
  against a plaintext server fails at once with version negotiation. The
  server warns unless the address is loopback, Tailscale's, or on a
  `tailscale*` or `wg*` interface; `known_hosts` refuses plaintext to a
  server known in TLS.
- **Takeover, view-only and reconnection:** a new controlling client
  takes over (the old one is told why); `--view-only` clients join beside
  it. Everything is encoded once, in a format every client decodes
  (`codec::shared`), and sent to all; only the controlling client's input,
  layout, mode, clipboard and text apply. The desktop client reconnects by
  itself after a loss (0.5 s doubling to 5 s), keeping its picture, and
  stops when the server says why: taken over, session ended, key refused.
- **Clipboard and text input** through the nested compositor
  (`ext-data-control`, `input-method-v2`, as for output scale):
  - MIME offers go over the control stream, and the data only when
    asked for, on a QUIC stream per transfer, either way.
  - The desktop client handles text (smithay-clipboard): it fetches the
    session's text when it changes and offers its own on focus.
  - IME commits and preedit go to the focused text field in the session,
    and the client hears when a text field gains or loses focus.
- **Kiosk mode** (`--app -- APP`): no nested compositor; the host
  composites the app and its popups itself (smithay's `Window` and a
  damage tracker into an offscreen picture) and applies the scale
  directly. The host offers `wl_data_device_manager`, without which GTK
  has no seat.
- **Audio out** (§8): the session's only sink is the server's own
  PipeWire stream, `farsight-speaker`. Each 5 ms graph cycle becomes an
  Opus frame (CELT low delay, 128 kbit/s), sent with the two before it.
  Nothing is encoded while nobody listens, and silence ends the stream.
  The client core buffers just enough for recent jitter, conceals loss,
  and corrects drift by playing up to 0.5% faster or slower; the desktop
  client plays through PipeWire with cpal. ALPN is `farsight/2`.

## Setup

As [M2](m2-results.md): the i7-6820HQ laptop, `iHD` VA-API, a headless
labwc standing in for the client's desktop. For audio, the client plays
into a private PipeWire of its own with only a null sink, recorded from
its monitor, so nothing reaches the laptop's speakers. Reproduce:

```sh
cargo build --release -p farsight-server -p farsight-desktop
(cd tools/m1/wltool && cargo build --release)
tools/m3/audio.sh /tmp/m3a                       # audio; SERVER_ARGS/CLIENT_ARGS="--no-tls ..." for plaintext
tools/m3/session.sh /tmp/m3s                     # takeover, view-only, outage, crash, exit
tools/m3/text.sh /tmp/m3t                        # clipboard both ways, text input
tools/m3/kiosk.sh /tmp/m3k                       # kiosk: menu popup, scale 2
```

## Audio

`tools/m3/audio.sh`: a 440 Hz tone played with `pw-play` in the session,
20–30 s. The client reports every 5 s: from the session's sink capturing
a sample to the client's sound server playing it, through the clock
offset measured by ping.

| | capture → speaker, p50 (p95) | buffer (target) |
|---|---|---|
| TLS, steady | 14.8–17.6 ms (15.1–18.5) | 9–11 ms (10.2) |
| plaintext, steady | 13.1–16.2 ms (15.4–17.1) | 9.5–11.6 ms (10.2) |

- That is mostly the jitter buffer (~10 ms: one 5 ms output request and
  5 ms of jitter allowance) and the client's output (2.7 ms as cpal
  reports it); the rest is Opus look-ahead, encoding and the network. The
  app's own buffer comes on top and isn't measured.
- **Within research's estimate** of 17–40 ms; video is never held for
  audio, so audio trails it by about this much.
- **Isolation:** the session's PipeWire has no devices
  (`pw-cli ls Device`: 0) and `farsight-speaker` is its default sink; the
  laptop's own PipeWire had the same 4 nodes before and during playback.
- **About once in 30 s on this loaded laptop, both sound servers stall
  for ~100 ms** (no realtime priority here: neither the session's PipeWire
  nor the user's own has any FIFO threads). The client conceals a frame or
  two, the buffer jumps by the backlog, and drift correction drains it in
  a few seconds. Under the unit, `LimitRTPRIO` lets PipeWire take
  realtime priority.
- In the recording, a 30 s tone came through as 30.08 s, with gaps only
  at those stalls.

## Session

`tools/m3/session.sh`, all as designed:

| Step | Result |
|---|---|
| A controls, B joins view-only | both decode; B's join renegotiates (one new epoch for A) |
| C connects | A closed with "another client took over"; B keeps watching |
| the server stops answering for 12 s (SIGSTOP) | B and C time out, keep their picture, and have reconnected 4 s after it answers again; the desktop is the same process |
| the desktop crashes (SIGSEGV) | restarted after 0.5 s; C stays connected and gets 2 new epochs |
| the desktop exits cleanly | the session ends; B and C print "the session ended" and exit |

**Under systemd:** `systemd-run --user` with the unit's settings
(`RuntimeDirectory=`, `InaccessiblePaths=`, `LimitRTPRIO=`, no
`XDG_RUNTIME_DIR`): the bus, PipeWire, WirePlumber, pipewire-pulse and
labwc run in the unit's cgroup, in `/run/user/1000/farsight-7793`; stop
leaves no process and no directory.

**Keys:** an unknown key is refused with the line to add; a changed
certificate is refused with both fingerprints and the `known_hosts` line
to remove; a key added while the server runs works on the next
connection.

## Clipboard and text

`tools/m3/text.sh` (`wltool` gains `copy`, `paste`, `commit` and
`keyboard`):

- Text copied on the client's desktop before the window has focus is
  pasted in the session: offered on focus, fetched only on paste.
- Text copied in the session is pasted on the client's desktop.
- Text reaches a terminal in the session both from the server's stdin
  (`text …`) and from an input method on the client's desktop, through
  the client window's text-input, the client, and the server's
  input-method; Enter through the window then runs the line.

## Kiosk

`tools/m3/kiosk.sh` with mousepad: the app fills the window; clicking
File opens its menu, composited at the right place, and a click elsewhere
closes it. At scale 2 the app redraws sharply at 2560×1600; the change
held frames 34 ms.

## Findings

- **GTK sets up no seat without `wl_data_device_manager`**: clicks reach
  the surface and nothing happens. The host now offers it.
- **smithay-clipboard learns of keyboard focus only from events it sees
  itself**, and can't set the selection without it; it has to exist
  before the window is mapped. And a headless test compositor has no
  keyboard, so no window ever gets keyboard focus: tests hold a virtual
  keyboard (`wltool keyboard`).
- **NVIDIA's EGL crashes destroying a window surface after the Wayland
  connection has closed**, which winit does before `run_app` returns. The
  desktop client drops its GL objects in `exiting`.
- **The server must say what goes first:** the control stream's first
  message must be `Welcome`, so the audio format, clipboard and
  text-input state follow it.
- **Audio packets pile up while the output opens** (~50 ms); playback
  starts at the target delay by dropping them, rather than draining them
  slowly.

## Left for later milestones

- **The client's keymap** still isn't applied: the session uses labwc's
  (M0). It belongs in labwc's XKB environment, which only takes effect
  at its start.
- **Kiosk apps get no clipboard sync or text input**: the host would
  have to be their data-device and text-input server itself.
- **A real system service and a user in `audio`**: the unit hides
  `/dev/snd` and WirePlumber has no devices, but neither was tried as
  root.
- **Other MIME types on the desktop client** (images): the protocol
  carries them; smithay-clipboard doesn't.
- **A LAN run between two machines**, and audio in the `tc netem`
  matrix (M4).
