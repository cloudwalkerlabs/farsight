# farsight

A low-latency remote desktop for headless Linux servers. The server is its own
headless Wayland compositor, and it streams hardware-encoded video over QUIC
datagrams. Clients run on the Linux desktop and on Android, and the Android
client is built to work with touch alone.

Early days: the Linux desktop client works end to end (M1): H.264 through
VA-API at both ends, input, and a client-side cursor. See
[`docs/design.md`](docs/design.md) for the design and milestones, and
[`docs/m1-results.md`](docs/m1-results.md) for what M1 measured.

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

Run a server with `farsight-server [--port 7740]`, and connect from a
Wayland desktop with `farsight-desktop HOST[:PORT]`. Both need VA-API (the
client falls back to software decoding). The server prints its certificate
fingerprint at startup; the client prints the one it sees, but doesn't pin
it yet. To keep a session running permanently, install
`dist/farsight-server@.service`; the file explains how.

The Android build runs `cargo ndk` itself and generates the Kotlin bindings,
so there is no separate Rust step.
