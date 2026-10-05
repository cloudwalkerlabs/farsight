# farsight

A low-latency remote desktop for headless Linux servers. The server is its own
headless Wayland compositor, and it streams hardware-encoded video over QUIC
datagrams. Clients run on the Linux desktop and on Android, and the Android
client is built to work with touch alone.

Early days: only the scaffolding exists. See [`docs/design.md`](docs/design.md)
for the design and milestones.

## Layout

```
crates/
  proto/           wire types (no I/O)
  net/             QUIC transport, FEC, congestion control
  server/          farsight-server: one isolated headless session per process; Linux only
  client/          platform-independent client core
  desktop/         desktop client
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

Run a server with `farsight-server [--port 7740]`. To keep a session
running permanently, install `dist/farsight-server@.service`; the file
explains how.

The Android build runs `cargo ndk` itself and generates the Kotlin bindings,
so there is no separate Rust step.
