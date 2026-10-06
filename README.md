# farsight

A low-latency remote desktop for headless Linux servers, with clients for
the Linux desktop and Android.

- **Smooth, sharp video:** hardware-accelerated on both ends, and stays
  sharp for text.
- **Fits your screen:** the remote desktop follows the client window's size
  and scale.
- **Holds up on bad networks:** lost packets don't freeze the picture or
  leave keys stuck.
- **A persistent session:** its own desktop that you can disconnect from
  and pick up again, from another device too; watch view-only alongside, or
  run a single app.
- **Sound and microphone:** the session's audio plays on the client, and
  apps in the session can use the client's mic, echo cancelled.
- **Clipboard and text input**, both ways.
- **Android:** touchpad or direct touch, pinch to zoom, an extra keys bar
  and the soft keyboard.
- **Secure by default:** only clients you authorize can connect.

See [`docs/design.md`](docs/design.md) for the design.

## Layout

```
crates/
  proto/           wire types (no I/O)
  net/             QUIC transport, FEC, congestion control
  server/          farsight-server: one isolated headless session per process; Linux only
  client/          platform-independent client core
  audio/           jitter buffer and playback, both ways
  desktop/         desktop client
  tiles/           tile coding, when the server has no hardware encoder
  va/              what a VA-API device encodes and decodes
  android/         Android bindings (uniffi)
  uniffi-bindgen/  Kotlin binding generator for the Android build
android/           Android app (Gradle, Compose)
dist/              example systemd unit
packaging/         packages: .debs, apt repository, PKGBUILDs, Homebrew formula
```

## Downloads

Each [release](https://github.com/cloudwalkerlabs/farsight/releases) has the Linux
server and desktop client (x86_64 and arm64), the desktop client for macOS
(arm64) and Windows, and the Android app. The Linux binaries need glibc
2.36 or later (Debian 12, Ubuntu 24.04, Fedora 37), PipeWire's library, and
libasound (the client) or libgbm and the XKB keymaps (the server); they
load libva and the GPU's libraries when they use them.

They also come as packages, which install those dependencies, and the
server's session daemons (dbus-daemon, PipeWire, WirePlumber and
pipewire-pulse) and `farsight-server@.service`:

- **Debian 12+ and Ubuntu 24.04+**, from farsight's apt repository, which
  keeps them updated:
  ```sh
  sudo install -d -m 755 /etc/apt/keyrings
  sudo curl -fsSLo /etc/apt/keyrings/farsight.gpg https://cloudwalkerlabs.github.io/farsight/apt/farsight.gpg
  sudo curl -fsSLo /etc/apt/sources.list.d/farsight.sources https://cloudwalkerlabs.github.io/farsight/apt/farsight.sources
  sudo apt update
  sudo apt install farsight-server     # or farsight-desktop
  ```
  labwc comes as a recommended package where the distribution has it
  (Debian 13, Ubuntu 24.04). Each release also has the .debs themselves.
  The repository is signed with the Cloudwalker Labs release key,
  `28FB E823 E55F 0E1B 03C8  4298 37F0 8092 1B1E 4AF7`
  ([`packaging/release-key.asc`](packaging/release-key.asc)).
- **Arch Linux:** `farsight-server-bin.PKGBUILD` and
  `farsight-desktop-bin.PKGBUILD` on each release, which repackage the
  .debs; save one as `PKGBUILD` in an empty directory and run `makepkg -si`.
- **macOS:** `brew install cloudwalkerlabs/tap/farsight-desktop`.

## Building

```sh
cargo build                      # server, desktop client, libraries
cargo test

cd android && ./gradlew assembleDebug                         # needs cargo-ndk
cd android && ./gradlew assembleDebug -Pfarsight.abis=arm64-v8a   # one ABI only
```

Run a server with `farsight-server [--port 7740]`, and connect from a
Wayland desktop with `farsight-desktop HOST[:PORT]`. First authorize the
client: `farsight-desktop --print-key` prints a line for
`~/.config/farsight/authorized_keys` on the server. The client pins the
server's certificate on first use. `--view-only` watches beside the
controlling client; `--no-tls` on both ends (with `--listen` on the
server) turns encryption off on networks that already encrypt, such as
Tailscale. The session's desktop is a Wayland compositor that the server
runs nested: labwc, or the command after `--`, such as
`farsight-server -- sway` or `farsight-server -- labwc --session
xfce4-session`. labwc and sway are tested; `docs/design.md` lists the
others. `farsight-server --app -- APP` runs a single app with no desktop
compositor. With `--mic`, the desktop client sends its microphone, echo
cancelled, while an app in the session records from `farsight-mic`.

The server encodes with VA-API or NVENC, and sends tiles without either;
the client decodes with VA-API or in software. Both build against FFmpeg
(the system's, or the small static one `tools/ffmpeg/build.sh` builds, as
releases do: set `PKG_CONFIG_PATH` to its `lib/pkgconfig`, which also has a
static libxkbcommon) and libva's and libdrm's headers, and build
libjpeg-turbo and libopus in; the server also needs dbus-daemon, PipeWire,
WirePlumber and pipewire-pulse, and the desktop's compositor (labwc unless
told otherwise). To keep a session running permanently, install
`dist/farsight-server@.service`; the file explains how.

The Android build runs `cargo ndk` itself and generates the Kotlin bindings,
so there is no separate Rust step. In the app, "This device's key" gives
its line for the server's `authorized_keys`.
