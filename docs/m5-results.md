# M5 — results

Status: measured 2026-10-05 on master. M5 adds the Android client
(MediaCodec, touch modes, viewport, extra keys, IME, AAudio, clipboard)
and the microphone on both clients: `farsight-mic`, `MicDemand`, and
capture with echo cancellation
([design.md, Milestones](design.md#milestones)).

**Verdict: done on one phone, over Tailscale, with gaps.** On a Galaxy
A32 (Android 13), the app shows the session at the screen's size and
density, decoded in hardware: 29–31 ms from the server's capture to the
display with NVENC H.264, at 93 fps. Keys, sticky modifiers, the soft
keyboard, touchpad motion, cursor images, tiles and the session's audio
(41–60 ms to the speaker) work. An app recording in the session gets the
phone's microphone, after the user allows it. With the platform's echo
cancellation, the session's pink noise comes back from the phone's
speaker at a median of about −74 dBFS rather than −43, with leaks of up
to −22 dBFS in a few half seconds, and a pure tone isn't cancelled at
all. The desktop client's AEC3 removes a 20 ms echo to the noise floor.
Not checked from here: anything with two or more fingers (adb can't),
typing through the soft keyboard itself, the clipboard on the phone, a
plain LAN, and a real call with someone talking. "Daily-usable" needs a
person with the phone.

## What M5 added

- **The microphone** (§8): the session's `farsight-mic` source runs only
  while an app records from it; the server then sends the controlling
  client `MicDemand(true)`. The client sends mono 10 ms Opus frames for
  voice (`VOIP`, 32 kbit/s, in-band FEC, the frame before repeated) as
  `Mic` datagrams, with capture times on the server's clock. The server
  plays them into the source through the same jitter buffer the client
  plays the session's audio through, now its own crate
  (`farsight-audio`), which rebuilds a lost frame from the next one's
  FEC. ALPN is `farsight/4`.
- **The desktop client's microphone**: `--mic` sends it, echo cancelled
  by WebRTC's AEC3 (sonora, a Rust port) unless `--no-echo-cancel`. The
  far end is what the client plays, queued with when it plays and lined
  up with the microphone by time.
- **The Android client** (`crates/android`, `android/`):
  - Rust keeps the hot paths. MediaCodec runs in asynchronous mode and
    decodes straight into the session view's `Surface`, which is handed
    over through JNI. Parameter sets go as codec config before each
    keyframe. `KEY_LOW_LATENCY` and the vendors' own keys are set. Frames
    render at once, and the frame-rendered callback (API 33, looked up at
    run time) times them. Tiles are drawn into the same `Surface` by the
    CPU, with libjpeg-turbo built from turbojpeg-sys's sources. AAudio
    plays the session's audio and records the microphone. A session
    reconnects by itself when the connection is lost.
  - Kotlin and Compose: an address book with thumbnails, per-server
    settings (touch mode, scale, fixed size, microphone never, ask or
    always, echo cancellation, sound, motion, view-only, plaintext), and
    this device's key to copy or share. The session runs full screen.
    The `SurfaceView` is placed and scaled by a local viewport: pinch,
    pan with inertia, a two-finger double tap for fit or 1:1, and
    following the cursor above the keyboard. There are touchpad and
    direct modes. A mouse and a hardware keyboard pass straight through.
    The soft keyboard goes as text when a field in the session has focus,
    and as keys otherwise. An extra keys bar has sticky modifiers, and a
    draggable toolbar hides itself. The clipboard works both ways, and
    there is a microphone prompt and indicator.

## Setup

The i7-6820HQ laptop of earlier milestones runs the server (VA-API on
the HD 530, NVENC on the Quadro M1000M), with labwc and an XFCE terminal
as the desktop. The phone is a Samsung Galaxy A32 (SM-A325F, MediaTek
Helio G80, Android 13, 2400×1080 at 420 dpi): `c2.mtk.avc.decoder` and
`c2.mtk.hevc.decoder`, no AV1. Both are on one 5 GHz Wi-Fi network, but
the laptop's firewall drops UDP from the LAN, so the phone connects over
Tailscale (WireGuard, a direct path). The phone was driven by adb: the
app takes `--es address HOST:PORT` to connect at once, and
`tools/m5/phone-echo.sh` runs the echo test.

The desktop microphone was measured on the laptop alone,
`tools/m5/mic.sh`. The client's private PipeWire has a virtual source
that hears the client's own output 20 ms late, as a speaker and a room
would, and later a 700 Hz tone as the near end.

## Video on the phone

The server picked HEVC (VA-API) first, H.264 with `--encoders nvenc`.
The figures are medians per second from the app's log. "Network" is the
server's capture to a frame's last datagram, so it includes encoding and
pacing; "total" is capture to the frame's release to the display.

| Server | Content | Network | Decode | Total | fps |
|---|---|---|---|---|---|
| NVENC H.264, 2400×1080 | es2gears, 90 fps | 17–20 ms | 11 ms | 29–31 ms | 93 |
| VA-API HEVC, 2400×1080 | es2gears, 90 fps | 54–56 ms | 12–13 ms | 67–70 ms | 65–72 |
| Tiles | a terminal | — | 4.5 ms | — | — |

- The VA-API rows are the server's limit, not the phone's: converting and
  encoding 2400×1080 in HEVC takes the HD 530 18–25 ms a frame while
  es2gears saturates the same GPU, so frames queue before they are sent.
- The round trip over Tailscale and Wi-Fi was 6–8 ms. The phone's Wi-Fi
  sleeps between packets (pings take up to a second when idle), but a
  stream keeps it awake: 3.9 ms at best, 15 ms on average with 50 pings
  a second.
- The MediaTek decoders take 11 ms whatever the low-latency keys say. The
  frame-rendered callback reports when the frame was released, not when
  it reached the panel, so the display's own vsync isn't counted.

## Audio on the phone

The session's audio went from 80–100 ms to the speaker to 41–60 ms when
AAudio's output buffer was cut from its default 2048 frames to two
bursts (512). Of the rest, about 20 ms is that output, and the jitter
buffer aims for 13–32 ms on this Wi-Fi. There were no underruns.

## The microphone

| Client | Capture → source | Notes |
|---|---|---|
| Desktop, loopback | 23–27 ms | jitter buffer 10–17 ms |
| Phone, Tailscale over Wi-Fi | 93–97 ms | the voice path's 20 ms bursts; jitter buffer ~50 ms |

**Echo, desktop** (`tools/m5/mic.sh`). The session plays pink noise for
8 s. The client plays it and hears it again 20 ms later at −32 dBFS. The
near end's tone is −32 dBFS. The table gives what a recorder in the
session got, in dBFS RMS per half second:

| | Echo (first second) | Echo (after) | Near end |
|---|---|---|---|
| No echo cancellation | −32 | −32 | −32 |
| AEC3 | −90 to −131 | median −121 to −150 (silence) | −32 to −33 |

In two runs of four, AEC3 let the echo through at −46 to −62 dBFS for a
second or two while it re-adapted, mid-stream.

**Echo, phone** (`tools/m5/phone-echo.sh`, the speaker at the phone's
default volumes). The session plays pink noise for 8 s, which the phone
plays and hears again. A recorder in the session got this, per half
second:

| | Before the far end | During it |
|---|---|---|
| No echo cancellation (`VOICE_RECOGNITION`, media output) | −56 to −58 | −41 to −44 |
| Platform canceller (`VOICE_COMMUNICATION`, communication mode) | −86 | mostly −86; median about −74; a few at −45, −36 and −22 |

A 1 kHz tone was not cancelled: it came back at −36 dBFS in
communication mode against −53 without, louder because communication
mode plays louder and adds gain. Voice cancellers often leave steady
tones alone. Whether the few loud windows with noise were leaks or the
room's own sounds could not be told from here. No near-end voice was
tested on the phone.

## Findings

- **The Android build had been broken since M3**: libopus's CMake build
  finds the NDK only through `ANDROID_NDK_HOME`, which cargo-ndk doesn't
  set. libjpeg-turbo needs the NDK's CMake toolchain file, and AAudio and
  libnativewindow need API 26+ libraries (the build now targets the
  app's minSdk, 30).
- **AEC3 must see the far end before its echo.** One frame late, its
  linear filter never converges, and six seconds in its transparent mode
  decides there is no echo (a headset) and stops suppressing. Counting
  samples to line up the far end with the microphone fails twice over: a
  far end dropped on a busy lock shifts it for good, and an output
  underrun plays silence the callback never saw. Timestamps line them
  up. A digital loopback with no delay is too tight a test; a speaker
  adds its own latency.
- **MediaCodec's `releaseOutputBuffer(render = true)`** takes the frame's
  timestamp as when to show it, and the frame-rendered callback reports
  that timestamp. Releasing at an explicit "now" fixes both.
- **AAudio's default output buffer** was 2048 frames (42 ms) on this
  phone, though the device's burst is 256 frames.
- **uniffi** objects already have `close()` (it frees them), so the
  session's own method is `disconnect()`. An error field named `message`
  clashes with `Throwable.message`.
- **Compose's `AndroidView`** keeps the first view it was given: a new
  session's view needs a `key`.
- **Asking for the microphone each time an app starts recording** was
  too much; an answer now holds for the session.
- **Back once** was too easy a way to leave a session; it takes two.
- **Samsung's first-run full-screen tip** covers the app once, until
  swiped away.

## Left for later

- **What adb can't do**: two- and three-finger gestures (scroll, pinch,
  right and middle click, the double tap), direct mode, the soft
  keyboard typing through the IME (composition and commits), a hardware
  keyboard and mouse, and rotation.
- **The clipboard on the phone**, both ways.
- **A plain LAN run**, and a move between Wi-Fi and LTE (QUIC
  migration).
- **A real call**: a near-end voice through the phone's canceller, double
  talk, and headphones; the leaks above; tones.
- **The phone's microphone latency**: the voice path delivers 20 ms
  bursts, and the jitter buffer sized itself for this Wi-Fi.
- **Keeping the session in the background**: the app closes it when it
  goes to the background and resumes it on return. Audio or a call would
  need a foreground service.
- **Real `wl_touch`** in direct mode, and pointer capture.
- **AV1 and 4:4:4** on Android: offered when present, but this phone has
  neither.
- **Switching from tiles to video** on one surface: the CPU stays
  connected to it, and MediaCodec may then fail to configure.
