# M0 spike — results

Status: measured 2026-10-05 on branch `m0-spike`. These numbers decide
whether option B (a host compositor with the desktop nested inside it,
[design.md](design.md#decision-a-host-compositor-with-the-desktop-nested-inside-it))
holds.

**Verdict: B holds.** labwc adds about 1.4 ms between an app's commit and
the host's. Its buffers reach the encoder with no copy. Scale works, but
needs `wlr-output-management`. Resize is not atomic. The keymap belongs to
labwc, not to the host seat.

## Setup

- Intel Core i7-6820HQ with HD Graphics 530 (Skylake GT2), `iHD` VA-API
  driver, render node `/dev/dri/renderD128`. The laptop's NVIDIA GPU was not
  used.
- labwc 0.20.2 (wlroots 0.20.2), Mesa 26.2.4, FFmpeg 9.0.2, Smithay 0.7.
- Pipeline: the nested buffer (dmabuf) → two GLES shader passes (BT.709
  limited range, into R8 luma and GR88 chroma views of a VA-API NV12
  surface) → `h264_vaapi` (CQP 24, no B-frames, `async_depth` 1) → `.h264`
  file. The spike waits on the CPU for the shader passes and for each
  encoded packet.
- The probe (`crates/server/examples/probe.rs`) is a wl_shm app inside
  labwc. It paints its frame number into its pixels and logs each commit in
  `CLOCK_MONOTONIC`. With `--probe`, the server reads that number back from
  each nested frame, which joins the two logs.

Reproduce:

```sh
cargo build --release -p farsight-server --bin farsight-server --example probe
tools/m0/latency.sh /tmp/m0 win_tick1080 1920x1080 100 0
```

## Latency: app commit → encoded H.264

Median (p95), in ms. *Windowed*: labwc composites the probe. *Tick*: the
probe repaints every 100 ms, as a typing user would cause.

| Scenario | app → host commit | convert | encode | **app → encoded** |
|---|---|---|---|---|
| 1080p windowed, tick | 1.37 (1.66) | 1.03 (1.10) | 2.93 (3.52) | **5.8 (6.6)** |
| 1080p windowed, animating | 7.07 (7.74) | 0.98 (1.06) | 3.04 (3.31) | 11.5 (12.6) |
| 4K windowed, tick | 1.36 (1.65) | 3.03 (3.27) | 8.73 (9.18) | **13.8 (14.4)** |
| 1080p fullscreen, tick | 2.41 (4.47) | 1.06 (1.81) | 3.36 (6.16) | 11.1 (17.9) |

- **labwc's hop costs about 1.4 ms** (an app commit, labwc composites, labwc
  commits to the host). That is the whole price of nesting in latency,
  which is small next to the 16 ms budget.
- **Import is free:** 0.04 ms. labwc renders into `XR24` with
  `I915_Y_TILED_CCS` (compressed), which imports as a texture directly.
  The encoder's surfaces map as `Y_TILED`, and the shader writes into them
  with no copy.
- **The encoder dominates.** The HD 530 needs about 3 ms for a 1080p frame
  and 9 ms for 4K. Newer GPUs are much faster.
- "Animating" is slower only because of queueing. The host sends frame
  callbacks right after conversion with no refresh cap, so the probe ran at
  about 200 fps, and the encode, which blocks the event loop, held labwc's
  next frame back. Moving the encode off the main thread (M1) and pacing to
  the client's refresh rate remove both causes.
- The host times include a 0.3–1 ms probe readback that only exists in the
  spike. It also absorbs the wait for labwc's GPU work, which a real
  pipeline would see as a GPU-side dependency instead.

### Direct scanout through the nesting

When an app is fullscreen, wlroots skips compositing and passes the app's
buffer straight through its Wayland backend to the host. The host then
receives the app's own buffer:

- A **dmabuf app** (most GPU-rendered apps) would go zero-copy from the app
  to the encoder, skipping labwc's composite.
- A **wl_shm app** (the probe) makes the host upload it: about 3 ms per
  1080p frame on this machine. That's why the fullscreen row above is
  slower than the windowed one.

Follow-up: decide whether the host should offer wl_shm at all. Without it,
labwc would have to composite shm apps on the GPU, which is faster than our
upload.

## Resize

- A runtime resize works: an xdg_toplevel configure with the new size →
  labwc changes its output mode → its first frame at the new size arrives
  about 15 ms later. Each size starts a new encoder (a new epoch).
- **labwc ignores the first configure** because it arrives before labwc
  enables its output. The host now repeats the configure once if the first
  frame has the wrong size.

## Fractional scale

- **The host's scale does not reach labwc.** labwc binds
  `wp_fractional_scale_manager_v1`, and the host sends `preferred_scale`
  and `preferred_buffer_scale`, but labwc's output stays at scale 1 and its
  apps never see a new scale.
- **`wlr-output-management` as a client of labwc works**
  (`wlr-randr --output WL-1 --scale 1.5`). The app gets
  `preferred_fractional_scale 1.5` and a matching logical size. This is the
  fallback planned in [design.md §5](design.md#5-resizing-and-hidpi-scaling);
  it is now the main path for labwc.
- **Rounding:** 1600 px at 1.5 is a 1066 px logical width, so the app draws
  1599 px. Prefer sizes the scale divides evenly, or accept a 1 px edge.

## Is a size + scale change one step? No

The open question from §5. The host sent a new size (1280×720 → 1800×1200)
and, straight after it, scale 1.5 through `wlr-output-management`:

- **2 frames were encoded at 1800×1200 that still held the app's old
  1280×720 content** before the first correct frame.
- The app also saw a transient configure (853×480 logical), because the
  scale reached labwc before the size did.

So the host must **hold the last good frame across a layout change**:
stop sending frames after `SetLayout` until labwc confirms the new output
state (output-management `done`) *and* a frame arrives after that, with a
timeout. The client already stretches the last frame meanwhile (§5).

## Keymap

- Keycodes, key state and modifiers pass through the host seat unchanged
  (evdev 21 with Shift → `Y`).
- **The host seat's keymap is ignored.** After the host switches its keymap
  to `de`, apps still get labwc's "English (US)" keymap. labwc takes its
  layout from its own XKB config: started with `XKB_DEFAULT_LAYOUT=de`,
  apps get "German" and evdev 21 → `z`.
- So the client's keymap must be applied to the **nested compositor's
  configuration**. At session start that's the environment; changing it
  later needs labwc's reconfigure (not tested yet). The host can't set it
  through the seat alone.

## Other findings

- The server must build the child's environment itself. A `WLR_BACKENDS`
  or `WLR_RENDERER` inherited from the caller stopped labwc from nesting;
  the spike strips `WLR_*` (§6).
- labwc picks the host's render node from linux-dmabuf feedback, as
  designed.
- wlroots binds `wp_presentation`, but the spike never sends presentation
  feedback. It's harmless for now; M1 should send it.

## What this changes in the plan

1. **B stands.** Nesting costs about 1.4 ms, and no blit is needed. A and A′
   stay as documented alternatives.
2. **§5:** use `wlr-output-management` for scale with labwc; hold frames
   across layout changes; repeat the first configure.
3. **§4/§7:** the client's keymap goes into labwc's XKB config, not the host
   seat.
4. **M1:** encode off the main thread; pace frame callbacks to the client's
   refresh rate; hand the conversion fence to VA-API instead of waiting on
   the CPU; send presentation feedback.
