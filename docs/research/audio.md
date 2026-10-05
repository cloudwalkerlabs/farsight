# Research: audio out, microphone in

Status: research, 2026-10-05, checked against master at 87476f1 (M1's
`farsight-proto`/`farsight-net`, and plaintext mode). Nothing here is
implemented. The isolation
and routing claims marked **(spike)** were checked on this machine with
[`tools/audio/isolated-session.sh`](../../tools/audio/isolated-session.sh).
The latency figures are estimates, not measurements.

**Questions:**
1. How does a session play audio without reaching the server's speakers or
   another session?
2. How does the client play it?
3. Can the client's microphone go back to the server?
4. How does audio travel over UDP, next to video and input?

**Short answer:**
- **Isolation works with what design §6 already plans,** plus four
  settings. Each session runs its own PipeWire in its private runtime dir.
  WirePlumber runs with its ALSA, Bluetooth and camera monitors off, so the
  session has **no hardware devices at all** (spike). `PULSE_SERVER` is set
  explicitly, and the service unit hides `/dev/snd`.
- **No null sink is needed.** `farsight-server` connects to the session's
  PipeWire and creates **its own sink and source** (two `pw_stream`s). The
  session picks them as its default devices (spike). Pulse, ALSA and native
  PipeWire apps all reach the sink (spike).
- **The microphone works the same way in reverse.** Our source node only
  runs while an app is recording (spike), so the server knows when to ask
  the client for its mic.
- **Audio goes in QUIC datagrams on the existing connection, with two new
  tags.** It uses Opus at 5 ms frames in CELT low-delay mode, with each
  packet repeating the previous frames. This replaces the plan of Opus
  in-band FEC, which doesn't exist in that mode (§4).
- **One blocker in the M1 transport:** quinn sends all datagrams through
  one FIFO, which M1 sized at 8 MB. An audio packet queued behind a
  keyframe waits for the whole keyframe to drain. `farsight-net` needs a
  priority scheduler in front of quinn (§4.3). No server or client code
  sends datagrams through `farsight-net` yet, so now is the cheap time to
  add it.

## 1. Isolation on the server

### What can leak, and what stops it

An app inside the session can reach a sound device by any of these routes.
"Accidental" is the threat model: these defences stop an app from finding
the wrong device by default. They don't stop a hostile process running as
the same user (see the end of this section).

| Route | Default behaviour | What stops it |
|---|---|---|
| Native PipeWire (`$XDG_RUNTIME_DIR/pipewire-0`) | Follows `XDG_RUNTIME_DIR` | The private runtime dir (design §6). Scrub `PIPEWIRE_REMOTE`, `PIPEWIRE_RUNTIME_DIR`, `PIPEWIRE_CONFIG_*` and `PULSE_*` from the environment; the server already builds the environment from scratch. **(spike)** |
| PulseAudio clients (libpulse) | Tries `$XDG_RUNTIME_DIR/pulse/native`, then the system-wide `/run/pulse/native`, and may autospawn `pulseaudio` | Set **`PULSE_SERVER=unix:<rundir>/pulse/native`** explicitly. Then libpulse neither falls back nor autospawns, and an app that starts before pipewire-pulse fails instead of finding another server. Audio also starts before the desktop (design §6, step 4). **(spike: `pactl` and `paplay` reached our sink)** |
| ALSA clients, `default` PCM | With **pipewire-alsa**, `default` is PipeWire, which follows `XDG_RUNTIME_DIR`. Without it, `default` is card 0. | Require pipewire-alsa (Arch: `pipewire-alsa`; Debian: `pipewire-alsa`). **(spike: the `pipewire` PCM reached our sink. pipewire-alsa isn't installed on this machine, so `default` tried card 0 and failed.)** |
| ALSA `hw:` and anything opening `/dev/snd/*` | `root:audio 0660`. logind adds a `uaccess` ACL **only for the active local seat**. A farsight session has no seat, so access fails unless the user is in `audio`. | Unit: `InaccessiblePaths=-/dev/snd`. By hand: at startup, warn if `/dev/snd/pcm*p` is writable. **(spike: `EACCES` for this user, which isn't in `audio`)** |
| WirePlumber's ALSA monitor | Opens every card, so the session would own the speakers | `hardware.audio = disabled` in our profile. Its device reservation (`org.freedesktop.ReserveDevice1`) runs on the session bus, and ours is private, so it couldn't even negotiate with the real desktop's PipeWire. **(spike: 0 devices)** |
| WirePlumber's BlueZ monitor | Talks to BlueZ on the **system** bus and registers A2DP/HFP endpoints, so it could take over headsets paired with the server | `hardware.bluetooth = disabled` **(spike)** |
| Cameras (V4L2, libcamera) | Exposed as Video/Source nodes | `hardware.video-capture = disabled` **(spike)**. A later camera redirection would add a virtual source, as the mic does. |
| JACK clients | pipewire-jack's libjack follows PipeWire. A real libjack finds a jackd for the same user through `/dev/shm`. | `JACK_NO_START_SERVER=1`, plus `JACK_DEFAULT_SERVER=farsight-<port>`, so a real libjack finds nothing |
| Network sinks (RAOP/AirPlay, zeroconf, RTP) | Only loaded if a config drop-in asks for them | Our config doesn't. The daemons get their own `XDG_CONFIG_HOME` (below), so the user's `~/.config/pipewire` drop-ins don't apply. Admin drop-ins in `/etc/pipewire` still do, which is deliberate. |
| Another farsight session, or the user's own desktop | Separate runtime dirs mean separate sockets **(spike: the user's `/run/user/1000` PipeWire was untouched)** | Shared *state* is the remaining leak: WirePlumber stores default devices and volumes in `$XDG_STATE_HOME/wireplumber`. Give the audio daemons a per-session state dir. |

**The WirePlumber profile** (WirePlumber 0.5; the feature names come from
`/usr/share/wireplumber/wireplumber.conf`):

```
wireplumber.profiles = {
  farsight = {
    inherits = [ main, mixin.systemwide-session ]  # no logind, reservation, portal store
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
  }
}
```

The server writes this file and runs `wireplumber --profile farsight`.

**Daemon-only environment.** pipewire, wireplumber and pipewire-pulse get
three variables that the desktop doesn't:
- `XDG_CONFIG_HOME=<rundir>/config`, holding only our drop-ins;
- `XDG_STATE_HOME=~/.local/state/farsight/<port>`, so volumes persist per
  session;
- the same private `DBUS_SESSION_BUS_ADDRESS` as everything else.

**Result in the spike:**
- The session had only `Dummy-Driver`, `Freewheel-Driver` and
  WirePlumber's fallback `auto_null` sink, with zero devices.
- `pw-metadata settings` showed rate 48000 and quantum 240.

**Socket path length.** dbus-daemon refused to start with a runtime dir under
the Claude scratchpad: "Socket name too long". `sun_path` is 108 bytes, and
the longest socket is `<rundir>/pulse/native`. design §6's choices are all
short (`/run/farsight-7740`, `/run/user/1000/farsight-7740`), but the
server should check the length at startup rather than fail obscurely.

**Fail closed under systemd.** These go in `dist/farsight-server@.service`,
which M3 owns:

```ini
RuntimeDirectory=farsight-%i
InaccessiblePaths=-/dev/snd -/run/pulse -/run/user/%U
Environment=JACK_NO_START_SERVER=1
LimitRTPRIO=88
LimitMEMLOCK=64M
```

- Hiding `/run/user/%U` makes the user's own desktop sockets unreachable,
  which is the one gap a stray environment variable could otherwise open.
- `/dev/dri` stays visible, so the encoder still works. Avoid
  `PrivateDevices=`, which hides `/dev/dri` too.
- The `LimitRTPRIO` and `LimitMEMLOCK` lines let PipeWire's `module-rt`
  take realtime priority without rtkit. rtkit's polkit rule normally grants
  it only to active local sessions, and we have none.

**Stronger isolation** (out of scope): the same UID can always connect to
any socket path it can name. Real separation between sessions means one
system user per session, or the namespace setup above. The table is
enough for "never by accident".

## 2. The server's sink and source

design.md's architecture box says "PipeWire null sink → Opus". A null sink
works, but then the server needs a separate capture stream from its
monitor ports. Simpler: **the server's own streams are the devices.**

| Node | `media.class` | `pw_stream` direction | Role |
|---|---|---|---|
| `farsight-speaker` | `Audio/Sink` | input: apps write into it | Encodes to Opus, sends to the client |
| `farsight-mic` | `Audio/Source` | output: apps read from it | Plays the client's decoded mic |

This is what `pw-record -P '{ media.class = "Audio/Sink" }'` and
`pw-play -P '{ media.class = "Audio/Source" }'` do. Both are plain
`pw_stream`s, and **(spike)**:

- WirePlumber made `farsight-speaker` the default sink as soon as it
  appeared, and moved `auto_null` out of the way.
- `paplay` (Pulse) and ALSA's `pipewire` PCM both reached it, at the
  source level (−21.1 dB max in and out).
- A `parecord` from the default source got the full 2 s tone from
  `farsight-mic`.
- **The speaker node only runs while something plays.** Recording for 6 s
  produced a 2.01 s file. When nothing plays, nothing is encoded or sent.
- **The mic node only runs while something records.** `pw-play` blocked
  until `parecord` linked to it.

Rust: the [`pipewire`](https://crates.io/crates/pipewire) crate (0.10.1)
covers `pw_stream`. The server runs a PipeWire thread loop next to the
Smithay event loop, and the realtime `process` callback only copies into a
lock-free ring. Opus encoding happens on our own thread.

**Graph clock.** With no hardware, the graph is driven by `Dummy-Driver`, a
timer on `CLOCK_MONOTONIC`. That's the clock the video `capture_us` already
uses, so audio and video timestamps are directly comparable. Setting
quantum = min = max = 240 at 48 kHz gives exactly one 5 ms Opus frame per
graph cycle, so the server never rebuffers. A fixed quantum costs a
headless server nothing, because there's no hardware buffer to tune
against.

**Lifetime.**
- Both nodes live as long as the session, **not** the client connection.
  If the sink vanished on disconnect, apps would move to `auto_null` and
  some would not move back.
- With no client connected, the speaker still consumes the audio but
  doesn't encode it.
- The mic outputs silence until a client sends audio.

## 3. The client

### Playback

| Platform | Output | Notes |
|---|---|---|
| Linux | [cpal](https://crates.io/crates/cpal) 0.18 (`pipewire` or `pulseaudio` host feature, ALSA otherwise) | Ask for a 5 ms buffer |
| Android | AAudio through cpal, or [oboe](https://crates.io/crates/oboe) 0.6 | `PERFORMANCE_MODE_LOW_LATENCY`, `SHARING_MODE_EXCLUSIVE` (MMAP where the device has it). Output latency varies by vendor; measure it, as for MediaCodec (design Risk 4). |
| macOS / Windows | cpal: CoreAudio / WASAPI | |

**Opus decoding:**
- **Recommended: libopus** through the [`opus`](https://crates.io/crates/opus)
  crate (0.4). libopus is 1.6.1 here. Android needs libopus built with the
  NDK, which the cargo-ndk build already handles for other C dependencies.
- **Not for now:** `opus-pure` exists but is young.
- **Not:** Android's MediaCodec Opus decoder adds buffering for no gain;
  decoding a 5 ms frame costs microseconds.

**Jitter buffer.** Unlike video (design §2, "no jitter buffer"), audio needs
one, because playback can't pause between packets.
- **Target:** about the p95 of inter-arrival jitter, at least one frame.
  Expect 5 ms on Ethernet and 10–20 ms on Wi‑Fi. Shrink it slowly and grow
  it fast.
- **Loss:** use the frame from a later packet's redundancy (§4) if it
  arrived in time. Otherwise use Opus PLC (`decode(None)`).
- **Clock drift:** the server's monotonic clock and the client's DAC drift
  apart by tens of ppm, which is seconds per day. A PI controller on the
  buffer's fill level drives an adaptive resampler, holding the ratio
  within ±0.1%, which is inaudible.
  - Libraries: [rubato](https://crates.io/crates/rubato), or speexdsp's
    resampler.
  - Don't drop or insert samples outside silence; the clicks are audible.

**Volume and mute.** The session's own volume (pavucontrol inside the
desktop) applies before encoding. The client has a local volume on top.
Client mute tells the server to stop sending, which saves bandwidth.

**A/V sync.** Video isn't delayed, so audio trails it by the jitter buffer
plus the output buffer, about 10–30 ms. ITU-R BT.1359 puts the detection
threshold for late audio at about 125 ms (45 ms for early audio). So
**don't hold video back for audio.** The shared timestamps (§4) let the
client measure the skew for telemetry.

### Latency estimate, app → speaker, LAN

| Stage | ms |
|---|---|
| App's own buffer (Pulse `tlength`, browser) | 10–40, not ours |
| Graph cycle (one frame) | 5 |
| Opus lookahead (CELT low-delay) + encode | 2.5 + <0.2 |
| Network one-way + scheduling | ~1 |
| Jitter buffer | 5–20 |
| Decode + output buffer | 3–10 desktop, 10–40 Android |
| **Total excluding the app** | **~17–40** |

We control the jitter buffer and the output buffer. Measure the rest in
M3, using the clock offset from the M1 Ping/Pong.

## 4. On the wire

### 4.1 Codec settings

design.md §8 says "Opus with 5–10 ms frames and in-band FEC". **Those two
don't go together:**

- Opus in-band FEC (LBRR) is a feature of the **SILK** layer.
- SILK needs frames of **at least 10 ms** and is off in
  `OPUS_APPLICATION_RESTRICTED_LOWDELAY`.
- 5 ms frames are therefore CELT-only, with no in-band FEC.

What to do instead:

| | Desktop audio (server → client) | Microphone (client → server) |
|---|---|---|
| Application | `RESTRICTED_LOWDELAY` (CELT only, 2.5 ms lookahead instead of 6.5 ms) | `VOIP` |
| Frame | 5 ms; 10 ms when the link is bad | 10 ms (20 ms when the link is bad) |
| Channels, rate | Stereo, 48 kHz | Mono, 48 kHz |
| Bitrate | 96–128 kbps | 24–32 kbps |
| Loss | **Packet redundancy:** each packet carries frame *n* plus *n−1* and *n−2* | In-band FEC (`set_inband_fec`, `set_packet_loss_perc` from measured loss), plus the same redundancy when loss is high. DRED (Opus 1.5+) is worth a look later; not evaluated. |
| Silence | Not sent: no packets while the node is idle (§2), and a flag when the frame is digital silence | DTX |

**Redundancy fits our design:**
- It's the same idea as input's "repeat the last N events" (design §4).
- It costs nothing in latency once the jitter buffer holds at least one
  frame, which it always does.
- At 128 kbps a 5 ms frame is 80 bytes, so three frames are about
  250 bytes per packet.

**Packet rate.** 5 ms frames mean 200 packets/s. QUIC, UDP and IP add
roughly 55–75 bytes to each packet, about 100 kbps on top of the audio.
The total is about 300 kbps. That's nothing next to video, but it's why
10 ms frames are the bad-link fallback.

**Plaintext mode** (`--no-tls`, design §1) changes little for audio:
- Each packet loses the 16-byte AEAD tag, about 26 kbps at 200 packets/s.
- The Opus payload is readable on the wire, so a tailnet must be trusted
  with the session's audio and the microphone, not just the screen.
  `--no-tls`'s warning text should say so.
- Nothing else changes: datagrams, priorities and the format below are
  the same.

### 4.2 Datagram format

M1's `farsight_proto::datagram` puts one tag byte in front of every
datagram. Tags 1–4 are taken (video, input, ping, pong). Audio adds
`TAG_AUDIO = 5` and `TAG_MIC = 6`, as two new `Datagram` variants:

```
TAG_AUDIO (server → client)   TAG_MIC (client → server)
┌──────┬───────┬───────┬────────────┬───────┬──────────────────────────────┐
│ tag  │ flags │ seq   │ capture_us │ count │ count × (len: u16, opus)     │
│ u8   │ u8    │ u32   │ u64        │ u8    │ newest first: seq, seq−1, …  │
└──────┴───────┴───────┴────────────┴───────┴──────────────────────────────┘
flags: bit 0 = silence (play silence, not PLC); bit 1 = discontinuity
       (stream restarted: reset the decoder and jitter buffer)
```

- **Hand-encoded, like `FragmentHeader`,** because it rides on 200 packets
  per second.
- **`seq` counts frames.** The frame duration is fixed per stream by the
  control message below, so no per-frame duration is needed.
- **`capture_us`** is the server's monotonic time at the first sample of
  frame `seq`, the same clock as video's `capture_us`. On the mic stream
  it's the client's clock, which the server maps through the Ping/Pong
  offset.

**Control-stream additions** to M1's `control.rs` (`Hello`,
`ClientMessage`, `ServerMessage`):

```rust
// In Hello
pub audio: Option<AudioCaps>,   // None: the client has no audio
pub struct AudioCaps { pub max_channels: u8, pub has_mic: bool }

// ServerMessage
AudioConfig { channels: u8, frame_us: u32 }        // also on a change of frame size
MicDemand(bool)                                    // an app started/stopped recording

// ClientMessage
SetAudio { play: bool, mic: bool }                 // mute, and the user's mic consent
```

### 4.3 Sharing the connection with video

Audio goes in **the same QUIC connection**: one port, one congestion
controller, and connection migration for free. A second connection would
compete with video blindly.

**Problem: quinn's datagram queue is a single FIFO.**
- `quinn-proto` 0.11.19 (`connection/datagrams.rs`) keeps one `VecDeque`
  of outgoing datagrams with no priority.
- M1 set `datagram_send_buffer_size` to 8 MB to hold keyframe bursts.
- Suppose a 300 KB keyframe is queued at a 20 Mbps send rate. The audio
  packet behind it waits **~120 ms**, and an input packet would too.

**Fix, in `farsight-net`:**
1. **A priority scheduler in front of quinn:**
   input/ping > audio > video. It keeps quinn's own buffer to roughly one
   pacing interval of data (check `datagram_send_buffer_space()`) and holds
   everything else in per-class queues. Only the video queue may grow or
   drop.
2. **Pace video, keyframes included,** at the controller's rate instead of
   handing over a whole frame at once. Otherwise the congestion window
   fills with video and audio waits for ACKs whatever its queue position.
3. **Reserve audio's ~300 kbps** off the controller's estimate before
   setting the video bitrate.

**Still open in M1's code** (`crates/net/src/endpoint.rs`): the 8 MB buffer
and no scheduler. Today only the loopback test calls `send_datagram`, so
the scheduler can go in before the server and client start sending.
**Input needs the same fix**, so this isn't audio-specific.

## 5. Microphone

**Yes, the client's mic can go back to the server.** The server side is
`farsight-mic` (§2). The flow:

1. An app starts recording. `farsight-mic`'s stream goes to `STREAMING`.
2. The server sends `MicDemand(true)`.
3. The client checks the user's setting (never / ask / always), shows a
   mic indicator, and opens the mic.
4. The recording stops: `MicDemand(false)`. The client closes the mic.

The mic is open only while something on the server is listening, which is
better for privacy and battery than an always-on mic.

**Echo.** If the client plays the session's audio through speakers, the
mic picks it up and sends it back, so the other side of a video call inside
the session hears itself. Echo cancellation belongs on the **client**: it
has the exact far-end signal and the real acoustic delay, and the server
sees network jitter on top.
- **Android:** AAudio input preset `VOICE_COMMUNICATION`, which turns on
  the platform's AEC and noise suppression where the device has them.
  Needs the `RECORD_AUDIO` permission, and a foreground service of type
  `microphone` to keep recording in the background.
- **Desktop:** [webrtc-audio-processing](https://crates.io/crates/webrtc-audio-processing)
  2.1 (AEC3), with what we play as the reference signal.
- **Headphones** don't need AEC. Let the user turn it off.

**Server side:** a jitter buffer, then into `farsight-mic`. The client's
mic clock drifts against `Dummy-Driver`, so the server corrects it the
same way PipeWire's own network modules (`module-rtp-source`,
`pulse-tunnel`) do: a delay-locked loop on the fill level adjusts the
stream's rate-match, and PipeWire's resampler applies it.

**Keyboard, mouse and touch** already go back over datagrams (design §4,
`farsight_proto::input` in M1). Nothing new is needed for them.

## 6. Where this goes in the plan

Applied to design.md (§1, §6, §8, Milestones, Risks):

| Milestone | Audio work | Why there |
|---|---|---|
| M1 | Datagram priority scheduler and video pacing in `farsight-net` | Input needs it as much as audio. Nothing sends datagrams yet, so it is cheapest now. |
| M3 | Audio out: isolated daemons, `farsight-speaker`, Opus with redundancy, desktop playback with jitter buffer and drift correction; unit file hardening | M3 already builds the runtime dir, private D-Bus, PipeWire and the unit file. Audio isolation is part of the same environment. |
| M4 | Redundancy depth, 10 ms fallback, audio in the `tc netem` matrix | Loss tuning for every channel is M4's job. |
| M5 | Android playback (AAudio). Microphone on both clients, with echo cancellation | The phone is where the mic matters most, and AEC needs a working playback path to test against. |

## Open questions

- **Real latency** from app write to client speaker, in particular how much
  pipewire-pulse adds for typical Pulse apps, and whether
  `pulse.min.quantum` and related settings can lower it.
- **Does an app holding a silent stream** (browsers do) keep the speaker
  node running? If so, silence detection matters more than node state for
  saving bandwidth.
- **More than one client** (design §6): encode once and send to each.
  Which client's mic wins? Probably only the controlling one.
- **Surround:** Opus multistream for 5.1. Later.
- **Is pipewire-alsa's routing** of `default` the same on Debian and Fedora
  as on Arch? Check before declaring it a dependency.

## Sources

- PipeWire 1.6.9 and WirePlumber 0.5.18 as installed: `/usr/share/pipewire/pipewire.conf`, `/usr/share/wireplumber/wireplumber.conf` (profiles, `mixin.systemwide-session`)
- quinn 0.11.12 / quinn-proto 0.11.19 source: `Connection::send_datagram`, `datagram_send_buffer_space`, `connection/datagrams.rs`
- [Opus codec (RFC 6716)](https://www.rfc-editor.org/rfc/rfc6716): SILK and CELT modes, frame sizes, LBRR
- [libopus API: `OPUS_APPLICATION_RESTRICTED_LOWDELAY`](https://opus-codec.org/docs/opus_api-1.5/group__opus__encoder.html)
- [PipeWire `module-rtp-source`](https://docs.pipewire.org/page_module_rtp_source.html) and [`pulse-tunnel`](https://docs.pipewire.org/page_module_pulse_tunnel.html): rate-matched network streams
- [systemd.exec: `InaccessiblePaths=`, `PrivateDevices=`](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
- [AAudio](https://developer.android.com/ndk/guides/audio/aaudio/aaudio): performance and sharing modes, input presets
- [ITU-R BT.1359](https://www.itu.int/rec/R-REC-BT.1359): audio/video timing thresholds
- Crates: [pipewire](https://crates.io/crates/pipewire) 0.10.1, [opus](https://crates.io/crates/opus) 0.4.0, [cpal](https://crates.io/crates/cpal) 0.18.2, [oboe](https://crates.io/crates/oboe) 0.6.1, [webrtc-audio-processing](https://crates.io/crates/webrtc-audio-processing) 2.1.0, [rubato](https://crates.io/crates/rubato)
