# M2 — results

Status: measured 2026-10-05 on master. M2 adds resize and scale, codec
negotiation, NVENC, HEVC, 4:4:4, idle refinement and, as decided during
M2, tiles instead of software video encoding
([design.md, Milestones](design.md#milestones)).

**Verdict: done, over loopback on one machine.** Drag-resizing the client
window and moving it to scale 1.5 and 2 both stay sharp, with video and
with tiles. Each layout change holds frames for 6–57 ms until the nested
compositor has drawn the new layout.

## What M2 added

- **Negotiation** (§3): the client lists its decoders by format (codec,
  chroma, bit depth, size limits, hardware or not). The server ranks the
  formats both ends share (hardware on both ends, then the mode's chroma,
  then codec efficiency), with tiles last. Every video epoch is announced
  in an `Epoch` message, and the client holds frames that overtake it.
  `SetMode` and `DecoderFailed` make the server pick again; a size past
  the first format's limits starts the next epoch in one that fits.
  ALPN is now `farsight/1`.
- **`farsight-va`**: asks libva what a device encodes and decodes, and up
  to what size.
- **Encoders**, hardware only:
  - VA-API through FFmpeg: H.264, HEVC and AV1, low-power where that is the
    only entrypoint. The conversion renders into the encoder's surfaces, as
    in M1.
  - **NVENC, bound directly** at API 12.1 (drivers 530 on). FFmpeg's NVENC
    asks for API 13.1 (driver 610+), which Maxwell and Pascal GPUs never
    get. libcuda and libnvidia-encode are loaded at run time. The host
    renders on its own render node, so the conversion is read back (NV12
    or planar 4:4:4) and copied into NVENC's input buffer.
- **Tiles** (`farsight-tiles`), when there is no hardware encoder, as VNC's
  Tight does: the damage, grown to a 64-pixel grid, is read back as RGBX
  and each tile goes as a fill, a palette of up to 256 colours with zlib
  (lossless), or TurboJPEG (4:4:4 in text mode). Tiles are packed whole
  into datagrams of at most 1100 bytes, so each datagram decodes alone. A
  lost update costs only the grid cells no datagram covered, which the
  client asks for again (`RequestRefresh`).
- **Resize and scale** (§5): `SetLayout` from the client, at most every
  50 ms during a drag. The server keeps sizes even and within the
  encoding's limits, and sets the nested compositor's scale through
  `wlr-output-management`, as a Wayland client of it. Frames are held
  until the nested compositor reports the new size and scale and commits a
  frame at that size (or 500 ms pass). The client stretches the last
  picture meanwhile, then draws 1:1.
- **Idle refinement** (§2): 250 ms after the last change the picture goes
  out once more: VA-API at `--refine-qp` (default 14) through a
  whole-picture region of interest, NVENC by reconfiguring its QP for one
  frame, tiles by sending their JPEG tiles again losslessly.
- **HiDPI cursors**: cursor images carry their density, and the client
  resamples them to the remote output's pixels.

## Setup

As [M1](m1-results.md): the i7-6820HQ laptop, `iHD` VA-API at both ends,
and a headless labwc standing in for the client's desktop. Its NVIDIA
Quadro M1000M (Maxwell, driver 580.178) runs NVENC. Reproduce:

```sh
cargo build --release -p farsight-server -p farsight-desktop
(cd tools/m1/wltool && cargo build --release)
tools/m2/resize.sh /tmp/m2                                # resize and scale
SERVER_ARGS="--encoders=" tools/m2/resize.sh /tmp/m2t     # the same, tiles
CLIENT_ARGS="--codec h264" tools/m1/e2e.sh /tmp/m2 gears  # latency, H.264
SERVER_ARGS="--encoders nvenc" tools/m1/e2e.sh /tmp/m2 gears
SERVER_ARGS="--encoders=" tools/m1/e2e.sh /tmp/m2 gears   # tiles
```

## What this machine can do

| | H.264 | HEVC | AV1 | 4:4:4 |
|---|---|---|---|---|
| VA-API encode (HD 530) | yes | yes | no | no |
| VA-API decode (HD 530) | yes | yes | no | no |
| NVENC (M1000M) | yes | no | no | H.264 |
| Software decode | yes | yes | dav1d | yes |

Without a hardware AV1 encoder here, the AV1 paths (VA-API, NVENC and the
client's dav1d) are written but not exercised.

## Latency

es2gears, 1600×900, 60 fps, median (p95) in ms. The server's columns are
from its frame log, the client's from its 5-second reports (as M1).

| Encoding | convert | encode | **commit → swap** | bytes/frame |
|---|---|---|---|---|
| VA-API H.264 | 0.8 (1.0) | 2.2 (2.4) | **10.4 (13.5)** | 1.5 K |
| VA-API HEVC | 0.8 (1.1) | 6.5 (6.8) | **14.3 (14.8)** | 2.4 K |
| NVENC H.264 (read back) | 4.9 (6.7) | 3.4 (3.6) | **17.1 (22.0)** | 2.4 K |
| Tiles | 1.4 (1.8) | 3.8 (4.4) | **10.5 (11.9)** | 8.7 K |

- **HEVC costs 4.3 ms more to encode than H.264 on this GPU**, yet wins
  negotiation, which ranks codec efficiency above latency (§3). On older
  Intel GPUs H.264 is the better choice; `--codec h264` on the client
  forces it. Ranking by a measured encode time is worth doing.
- **NVENC's read back dominates:** 4.9 ms to convert on the Intel GPU and
  read 1600×900 back. Rendering on the NVIDIA GPU and handing the picture
  to CUDA would remove it.
- **Tiles keep up with es2gears** at 60 fps, at about 4 Mbit/s for its
  small window. Typing into a terminal costs 1.4–1.9 ms from commit to
  encoded and 1.5–3 KB an update. A whole 1600×900 terminal is 16 KB; a
  noisy photo is 1 MB and 270 ms to code, so full-screen video stays the
  tiles' worst case, as in VNC.

## Resize and scale

`tools/m2/resize.sh`: the client window grows by 100 px five times and
shrinks back, then the client's output goes to scale 1.5, 2 and back to 1.

| Change | frames held (H.264) | (tiles) |
|---|---|---|
| width ±100 px, ten times | 6–38 ms | 11–18 ms |
| scale 1 → 1.5 (1920×1200) | 32 ms | 35 ms |
| scale 1.5 → 2 (2560×1600) | 57 ms | 56 ms |
| scale 2 → 1 (1280×800) | 18 ms | 18 ms |

- The hold always ended on the nested compositor's confirmation, never on
  the 500 ms timeout.
- At every step the client's picture is the window's size, drawn 1:1, and
  the terminal in the session redraws at the new scale: text is as sharp
  at 2× as at 1×.

## Idle refinement

- VA-API H.264 at `--qp 40`: a terminal's text is visibly blocky; the
  refinement frame (5 KB) arrives 250 ms after typing stops and the text
  is clean after it. NVENC: 5.7 KB, the same.
- Tiles: a gradient photo goes out as 54 KB of JPEG, then 181 KB of
  lossless tiles once idle. Text and UI are never lossy in the first
  place.

## Findings

- **The host must never wait on the nested compositor.** The first version
  connected to labwc's socket with a blocking roundtrip from inside the
  handler for labwc's own request, while labwc waited on the host:
  deadlock. The connection now only sends, and handles events from the
  event loop.
- **labwc draws its cursor at the output's scale with a buffer scale of 1**
  (24, 36, 48 px at 1, 1.5, 2), so its images arrive at the output's
  density already. The client only resamples for compositors that use
  `buffer_scale` or a viewport.
- **TurboJPEG writes about 440 bytes of Huffman tables and JFIF header
  into every JPEG.** Tiles drop them, and libjpeg-turbo restores the
  standard tables when decoding, as it does for Motion JPEG. Without that,
  a tile small enough for one datagram would be half headers.
- **Tiles that don't fit a datagram end up lossless:** a JPEG tile too big
  is split, and at 16×16 a tile has at most 256 colours, so it becomes a
  palette. Very noisy pictures are therefore sent losslessly, at a cost.
- **iHD honours regions of interest in CQP**: a whole-picture offset of
  −0.3 makes frames 3.6 times bigger, which is what refinement relies on.

## Left for later milestones

- **The mode can't be switched from the client's window yet**: it is
  `--mode` at start. `SetMode` works on the server.
- **No decode latency in `DecoderCaps`** (§3 asks for a test decode), and
  no ranking by encode latency.
- **VA-API 4:4:4 encoding** needs the conversion to write packed AYUV;
  this GPU has none to test it on.
- **NVENC without the read back**, when the host renders on the NVIDIA GPU.
- **The keymap still belongs to labwc** (M0); applying the client's keymap
  is M3's, with the session.
- **A LAN run between two machines** is still to do (M1, M4).
