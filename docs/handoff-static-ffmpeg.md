# Handoff: link a minimal FFmpeg statically

Status: 2026-10-05, not started. Branch `static-ffmpeg`.

## Goal

Release builds stop depending on the system's FFmpeg. Each platform's
binary links a small FFmpeg of our own, built in CI from a pinned version,
statically.

Today:

- **Linux** (`farsight-server`, `farsight-desktop`): links the system's
  FFmpeg 9, so the tarball runs only where FFmpeg 9 is installed (it is
  built in an `archlinux` container).
- **macOS** (`farsight-desktop`): links Homebrew's FFmpeg; users must
  `brew install ffmpeg`, which pulls in dozens of libraries.
- **Windows** (`farsight-desktop`): links BtbN's full LGPL shared build
  (`ffmpeg-n9.0-latest-win64-lgpl-shared-9.0`), and the zip carries every
  DLL, about 70 MB.
- **Android** doesn't use FFmpeg (MediaCodec). Nothing to do.

## Why static is fine

FFmpeg is LGPL. Static linking obliges us to let users relink against a
modified FFmpeg; farsight's source is public, so rebuilding from it
satisfies that. What remains:

- Ship FFmpeg's licence (`COPYING.LGPLv2.1`) and a notice with the FFmpeg
  version and configure line in each archive.
- Never pass `--enable-gpl` or `--enable-nonfree`. Nothing we need
  requires them: dav1d is BSD, the hardware codecs are LGPL.

## What farsight uses of FFmpeg

Only **libavcodec** and **libavutil** (checked by grepping `ff::` in
`crates/server/src/encode/` and `crates/desktop/src/decode.rs`). No
avformat, avfilter, avdevice, swscale or swresample.

- **Server** (`crates/server/src/encode/{ffmpeg,vaapi}.rs`, Linux only):
  encoders `h264_vaapi`, `hevc_vaapi`, `av1_vaapi`; hwcontexts VAAPI and
  DRM (it maps DRM PRIME frames into VA-API with `av_hwframe_map`, so
  FFmpeg needs `--enable-libdrm`). NVENC is bound directly, not through
  FFmpeg.
- **Desktop client** (`crates/desktop/src/decode.rs`): decoders `h264`,
  `hevc`, `av1` (hardware only) and `libdav1d` (software AV1); hwaccels
  per platform: VA-API (Linux), VideoToolbox (macOS), D3D11VA (Windows).
  Also `av_md5_sum` (avutil) and the log callback.

## Plan

1. **A build script**, `tools/ffmpeg/build.sh`, that builds a pinned
   FFmpeg (and dav1d) into a prefix with static libraries and `.pc`
   files. Roughly:

   ```sh
   ./configure --prefix="$PREFIX" --enable-static --disable-shared \
     --disable-everything --disable-programs --disable-doc \
     --disable-avformat --disable-avfilter --disable-avdevice \
     --disable-swscale --disable-swresample --disable-network \
     --enable-pic \
     --enable-libdav1d --enable-decoder=h264,hevc,av1,libdav1d \
     --enable-parser=h264,hevc,av1 \
     # Linux:   --enable-vaapi --enable-libdrm \
     #          --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi \
     #          --enable-encoder=h264_vaapi,hevc_vaapi,av1_vaapi
     # macOS:   --enable-videotoolbox --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox
     # Windows: --enable-d3d11va --enable-hwaccel=h264_d3d11va,hevc_d3d11va,av1_d3d11va
   ```

   Check after `configure` that the list it prints has exactly these, and
   that `--disable-everything` hasn't also dropped bitstream filters or
   anything the encoders need (`h264_metadata` and the like aren't used
   today). dav1d builds with meson and ninja, statically
   (`-Ddefault_library=static`).

2. **Point ffmpeg-sys-next at it.** In `crates/server/Cargo.toml` and
   `crates/desktop/Cargo.toml`:

   ```toml
   ffmpeg-sys-next = { version = "9", default-features = false, features = ["avcodec", "static"] }
   ```

   `avutil` is always linked. Don't use ffmpeg-sys-next's own `build`
   feature: it runs FFmpeg's configure with everything enabled and has no
   way to pass `--disable-everything`.

   Link through **pkg-config, not `FFMPEG_DIR`**: with `FFMPEG_DIR` and
   `static`, the build script links only the `libav*` archives and none
   of their dependencies (libva, libdrm, dav1d, the macOS frameworks),
   while pkg-config with `static` adds each `.pc`'s `Libs.private`. So in
   CI set `PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig` and leave `FFMPEG_DIR`
   unset. Local builds without that variable keep using the system's
   FFmpeg, which is what a developer on Arch wants.

   Keep these two decisions in the build config, not in Rust: nothing in
   the source needs to change.

3. **CI** (`.github/workflows/build.yml`): a step before `cargo build`
   that restores the prefix from `actions/cache` (key: OS, the FFmpeg and
   dav1d versions, and a hash of `tools/ffmpeg/build.sh`), or runs the
   script. Expect 5–10 minutes uncached. Then:
   - **Linux**: drop `ffmpeg` from the `pacman` line in both workflows
     only for the release build (the PR test can keep the system's).
     libva and libdrm stay dynamic, from the system: their drivers come
     from there anyway.
   - **macOS**: drop `brew install ffmpeg`; install `nasm meson ninja`
     instead. Remove the `brew install ffmpeg` note from README's
     Downloads section.
   - **Windows**: the hard part. FFmpeg's configure needs a Unix shell,
     so use `msys2/setup-msys2` with the MSVC toolchain (`--toolchain=msvc`
     from an MSYS2 shell with the MSVC environment loaded, e.g. via
     `ilammy/msvc-dev-cmd`). If that's too painful, fall back to keeping
     BtbN's build but shipping only `avcodec-*.dll` and `avutil-*.dll`
     (and whatever they import: check with `dumpbin /dependents`), which
     cuts the zip to a few MB. Then drop `FFMPEG_DIR` and the "Fetch
     FFmpeg" step if static works.

4. **Licence notice**: add `COPYING.LGPLv2.1` from the FFmpeg source and a
   short `FFMPEG.txt` (version, configure line, where the source is) to
   each archive in the Package steps.

## How to check it

- `ldd target/release/farsight-desktop` (Linux) lists no `libav*`;
  `otool -L` (macOS) lists only system frameworks and `/usr/lib`;
  `dumpbin /dependents` (Windows) lists no `av*.dll`.
- Linux: run the server and client end to end as in the earlier
  milestones; the server must still encode through VA-API (look for the
  encoder it picked in its log), and the client must decode in hardware
  (`decoder ready … hardware=true`) and in software with `--software`,
  including AV1 through dav1d.
- macOS and Windows: no machine here; at least check the binary starts
  and prints `--help` in CI (`farsight-desktop --help`), which catches
  missing symbols at load.
- Binary size: should grow by a few MB, not tens.

## Related, not part of this

- **The Linux tarball also needs a recent glibc**, being built on Arch.
  A build in an older container would run on more distros, but tiles use
  TurboJPEG 3's API (`tj3*`), and e.g. Ubuntu 24.04 ships libjpeg-turbo
  2.1; that would need the bundled one (as macOS and Windows use, see
  `crates/tiles/Cargo.toml`) on Linux too.
- **The Android APK is signed with a throwaway debug key** each CI run,
  so releases can't update one another. A keystore in repository secrets
  fixes it.
