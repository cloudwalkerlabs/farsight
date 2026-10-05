# M1 — results

Status: measured 2026-10-05 on master. M1 is the first end-to-end build: the
server streams its nested desktop to the Linux desktop client, which sends
input back ([design.md, Milestones](design.md#milestones)).

**Verdict: works end to end, measured over loopback on one machine.** From
the nested compositor's commit to the client's buffer swap takes **10 ms**
(1600×900, 60 fps). Input and pings don't queue behind keyframes. A run
between two machines on a real LAN is still to do.

## What M1 added

- **Transport** (`farsight-net`): a datagram priority scheduler in front of
  quinn (`sched.rs`), as in [§1](design.md#1-transport). Input and ping go
  first, then audio, then video. Video is paced, keyframes included, and
  quinn's buffer holds at most about 2 ms of it. A keyframe supersedes the
  video frames queued before it, and a backlog over 250 ms drops the oldest.
  With no congestion controller yet (M4), video is paced at a fixed
  `--rate` (100 Mbit/s by default).
- **Server:**
  - A network thread (`net.rs`) serves one client at a time; a new
    connection takes over the session.
  - The conversion stays on the main thread. Encoding moved to its own
    thread, which waits for the conversion's fence (`pipeline.rs`).
  - Frame callbacks are paced to the client's refresh rate. Presentation
    feedback is sent.
  - A client that connects, or asks for a keyframe, gets the current screen
    as an IDR at once, even when the desktop is idle.
  - Input goes into the host seat (`input.rs`). The nested compositor's
    cursor, named or a surface, goes to the client (`cursor.rs`).
- **Client core** (`farsight-client`):
  - connection, hello and welcome;
  - frame reassembly, and keyframe requests after a loss: frames are
    dropped until a keyframe arrives, so the decoder never sees a damaged
    stream;
  - input with history and snapshots;
  - clock offset from ping/pong.
- **Desktop client** (`farsight-desktop`):
  - winit, with GLES 3 through glutin;
  - FFmpeg H.264 decode on VA-API, or in software;
  - NV12 → RGB in a shader; no vsync wait;
  - evdev keys, buttons, scroll and absolute pointer;
  - the remote cursor becomes the window's own cursor (a winit custom
    cursor or a named shape), so moving it has no latency;
  - per-stage latency logged every 5 s.

## Setup

- The same laptop as [M0](m0-results.md): i7-6820HQ with HD Graphics 530,
  `iHD` VA-API at both ends.
- The client's desktop is a headless labwc on the same render node,
  standing in for a monitor. `tools/m1/wltool` drives it: screenshots,
  virtual pointer, virtual keyboard.
- Client and server talk over 127.0.0.1, so the "network" stage below is
  scheduling and the stack, not a wire.

Reproduce:

```sh
cargo build --release -p farsight-server -p farsight-desktop
(cd tools/m1/wltool && cargo build --release)
tools/m1/e2e.sh /tmp/m1 gears       # latency
tools/m1/e2e.sh /tmp/m1 keyframes   # input under keyframes
tools/m1/e2e.sh /tmp/m1 input       # type through the client, screenshot
```

## Latency

es2gears in the session, 1600×900, 61 fps. Median (p95) in ms, from the
client's 5-second reports:

| encode | network | decode | present | **total** |
|---|---|---|---|---|
| 3.1 (3.4) | 0.4 (0.5) | 5.5 (6.0) | 1.0 (1.2) | **10.1 (10.7)** |

- **total** runs from the nested compositor's commit on the server to the
  client's buffer swap. The server's timestamp is converted with the clock
  offset from ping. The client compositor's wait for its next refresh and
  the display's scanout come on top.
- **encode** is commit → encoded packet: the conversion pass and the
  encoder, as in M0.
- **decode** includes copying the decoded VA surface to memory. **present**
  is the texture upload and the draw. Importing the decoded dmabuf into EGL
  would remove the copy and most of the upload; that is later work.

### Pacing frame callbacks costs an animating app up to one refresh

The M0 probe, animating on frame callbacks (`tools/m0/latency.sh … ""
0`), now goes from app commit to encoded in 20.3 ms (p95 20.8), against
M0's 11.5 ms. 16 ms of that is the app's commit waiting for labwc's next
composite, which now comes at the client's refresh rate (60 Hz) instead of
as fast as the encoder allows (about 200 fps in M0). A local 60 Hz display
imposes the same wait. Typing-style updates are unchanged: 5.9 ms (p95
6.3), as in M0.

## Input under keyframes

The goal: input latency doesn't rise during keyframes. Pings use the
input class, so the round trip of a ping stands in for input.

| Scenario | ping RTT median | ping RTT max |
|---|---|---|
| gears, no keyframes | 0.2–0.5 ms | 0.61–0.71 ms |
| 120 KB keyframe every 100 ms, paced at 20 Mbit/s (48 ms each) | 0.3 ms | 0.65–0.66 ms |

Without the scheduler, a ping sent during a keyframe would wait in quinn's
FIFO for the rest of the frame: up to 48 ms here. The `pings_overtake_a_keyframe`
loopback test checks the same thing with a 1 MB keyframe at 20 Mbit/s
(400 ms): every ping returns in under 20 ms.

## Findings

- **wlroots' Wayland backend ignores the position in `wl_pointer.enter`.**
  The nested compositor's cursor stayed where it was until the next
  motion, so a click right after entering landed elsewhere. In the first
  test, it opened labwc's root menu, and the next key chose "Exit". The
  host now follows every enter with a motion.
- **The iHD driver crashes at exit if a VA context is still open**: a
  double free in its exit-time destructors. Both the server's encode thread
  and the client's decode thread are now joined, and their codecs dropped,
  before the process exits.
- **labwc aborts when its host compositor goes away** (an assertion in
  `wlr_backend_finish`). Harmless today, since the server is exiting
  anyway, but desktop supervision (M3) must expect it.
- **Clients drop keys that arrive while they load a new keymap.** This only
  bit the test tool, which uploads a keymap on each run; it now waits
  200 ms first. The real path has no keymap changes in M1. The keymap is
  still labwc's own, as M0 found.

## M0's follow-ups

| From M0 | State |
|---|---|
| Encode off the main thread | Done |
| Pace frame callbacks to the client's refresh rate | Done |
| Send presentation feedback | Done |
| Hand the conversion fence to VA-API | Not possible through FFmpeg's VA-API encoder, which takes no fence. The encode thread waits on it instead, off the main thread. |

## Left for later milestones

- **A LAN run between two machines.** Loss and congestion behaviour need a
  real link anyway (M4's `tc netem` matrix).
- **No congestion control, FEC or NACK** (M4). A lost frame costs a
  keyframe.
- **`SetLayout` is ignored** (M2). The layout in `Hello` is applied at
  connect, and the client letterboxes the video when its window changes
  size.
- **Cursor images aren't scaled for HiDPI.** The client shows them at
  their buffer size (M2, with scale).
- **The server's certificate isn't pinned yet** (M3). The client prints the
  fingerprint.
