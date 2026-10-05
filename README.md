# farsight

A low-latency remote desktop for headless Linux servers. The server is its own
headless Wayland compositor, and it streams hardware-encoded video over QUIC
datagrams. Clients run on the Linux desktop and on Android, and the Android
client is built to work with touch alone.

Early days: the Linux desktop client works end to end, with input and a
client-side cursor (M1). The remote desktop follows the client window's
size and scale, and the two ends negotiate the encoding (M2): H.264 or HEVC
through VA-API or NVENC, 4:4:4 where the hardware has it, and VNC-style
tiles (TurboJPEG, palettes) when the server has no hardware encoder. The
server is an isolated session (M3): its own D-Bus and PipeWire, audio
played on the client, clipboard and text input, client keys, plaintext
mode for tailnets, reconnection, takeover and view-only clients, and a
kiosk mode for a single app. See [`docs/design.md`](docs/design.md) for
the design and milestones, and [`docs/m1-results.md`](docs/m1-results.md),
[`docs/m2-results.md`](docs/m2-results.md) and
[`docs/m3-results.md`](docs/m3-results.md) for what was measured.

## Layout

```
crates/
  proto/           wire types (no I/O)
  net/             QUIC transport, FEC, congestion control
  server/          farsight-server: one isolated headless session per process; Linux only
  client/          platform-independent client core
  desktop/         desktop client
  tiles/           tile coding, when the server has no hardware encoder
  va/              what a VA-API device encodes and decodes
  android/         Android bindings (uniffi)
  uniffi-bindgen/  Kotlin binding generator for the Android build
android/           Android app (Gradle, Compose)
dist/              example systemd unit
```

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
desktop.

The server encodes with VA-API or NVENC, and sends tiles without either;
the client decodes with VA-API or in software. Both need FFmpeg,
libjpeg-turbo and libopus; the server also needs dbus-daemon, PipeWire,
WirePlumber and pipewire-pulse, and labwc for the default desktop. To
keep a session running permanently, install
`dist/farsight-server@.service`; the file explains how.

The Android build runs `cargo ndk` itself and generates the Kotlin bindings,
so there is no separate Rust step.
