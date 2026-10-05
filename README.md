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
```

## Downloads

Each [release](https://github.com/simophin/farsight/releases) has the Linux
server and desktop client, the desktop client for macOS (it needs
`brew install ffmpeg`) and Windows, and the Android app.

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
Tailscale. `farsight-server --app -- APP` runs a single app with no
desktop. With `--mic`, the desktop client sends its microphone, echo
cancelled, while an app in the session records from `farsight-mic`.

The server encodes with VA-API or NVENC, and sends tiles without either;
the client decodes with VA-API or in software. Both need FFmpeg,
libjpeg-turbo and libopus; the server also needs dbus-daemon, PipeWire,
WirePlumber and pipewire-pulse, and labwc for the default desktop. To
keep a session running permanently, install
`dist/farsight-server@.service`; the file explains how.

The Android build runs `cargo ndk` itself and generates the Kotlin bindings,
so there is no separate Rust step. In the app, "This device's key" gives
its line for the server's `authorized_keys`.
