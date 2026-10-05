# M4 — results

Status: measured 2026-10-05 on master. M4 makes the stream hold up on a
lossy, congested network: congestion control, FEC, NACK on quick paths,
reference frame invalidation, audio redundancy that follows the loss, the
10 ms audio fallback, and a `tc netem` test matrix
([design.md, Milestones](design.md#milestones)).

**Verdict: done, over loopback under `tc netem`.** At 5% random loss,
nothing is lost after FEC, every picture the client decodes is bit for
bit what a decode of the whole stream gives, every typed line arrives
exactly, and no audio frame goes missing once the buffer has measured the
loss (about 10 ms more of it). The same holds at 10%, with 5% at a 20 or
80 ms round trip, and behind an 8 Mbit/s bottleneck. Bursty loss beats
FEC now and then; the client then decodes nothing damaged and recovers by
NACK on a quick path, or by RFI. Not done: a run between two machines,
or over Wi-Fi; audio under bursty loss still drops a frame now and then.

## What M4 added

- **FEC** (§2, `farsight_net::packetize`): each frame is split into equal
  shards and followed by Reed-Solomon parity (reed-solomon-simd); any
  `data` of its shards rebuild it. The parity is the fewest shards that
  leave a frame lost at most 0.2% of the time (0.05% for keyframes), for
  independent loss at 1.5 times the rate quinn measures on the worst
  connected path, which starts at 2% until measured. ALPN is `farsight/3`.
- **Congestion control** (§1, `farsight_net::cc`): quinn's controller is
  replaced by one whose window follows a delay-based rate, decided every
  50 ms from the queue the stream builds over the base round trip, and
  only heavy loss. The ack-frequency extension asks the client to ack
  within 2 ms. The rate paces each connection's video; the encoder aims
  for the slowest connection's share, through a QP offset on the
  constant-QP encoders (NVENC reconfiguration, a VA-API region of
  interest over the whole picture). While over 20 ms of video is queued,
  the pipeline skips encoding new frames. `--rate` is now the most
  video may use.
- **NACK** when a round trip and two shards' time fit in a frame: the
  client asks for a stalled frame's missing shards, or a whole frame it has
  none of, and holds the frames behind it meanwhile; the server keeps the
  last 32 frames and sends repairs paced, ahead of new video.
- **RFI** otherwise: fragments carry `refs`, the newest frame each may
  reference; the client decodes only frames whose references it has, and
  asks the server to predict from its last good frame. NVENC invalidates
  the frames after it (frame numbers are its timestamps, 8 frames kept for
  reference); VA-API, through FFmpeg, answers with a keyframe.
- **Audio** (§8): two to five repeats per datagram, by the lossiest
  listener's loss; the client buffers for the repeats it needs; 10 ms
  frames at 64 kbit/s on a path under 1.5 Mbit/s.
- **Input** (§4): standalone snapshots carry the recent events, so a tap
  lost just before a pause is still typed.
- **Tiles:** an update still incomplete 3 ms after a later one shows up
  has its missing cells asked for at once, rather than after 250 ms.
- **MTU:** fixed at 1200; path MTU discovery is off (Findings).
- **Checks:** the desktop client counts pictures FFmpeg finds damaged and
  the errors it logs, and with `FARSIGHT_FRAME_MD5` hashes every picture
  it decodes.

## Setup

The i7-6820HQ laptop of earlier milestones: VA-API on the HD 530 (`iHD`),
NVENC on the Quadro M1000M (Maxwell, which has reference invalidation).
`tools/m4/netem.sh` runs the server, the client, the client's headless
desktop and its private sound server in a user and network namespace of
their own (`unshare -rn`, no root needed) with `tc netem` on its
loopback. Loopback carries both directions, so the impairment applies
each way: `loss 5%` loses 5% of the packets each way, `delay 10ms` is a
20 ms round trip. Jitter is given with a rate (`delay 10ms 2ms rate
1gbit`), since without one netem reorders freely, which real links seldom
do.

```sh
cargo build --release -p farsight-server -p farsight-desktop
(cd tools/m1/wltool && cargo build --release)
tools/m4/netem.sh /tmp/m4 "loss 5%"                      # one run; CHECK_FRAMES=1, TYPE_LINES=20
tools/m4/matrix.sh /tmp/m4m                              # the matrix below
```

Each condition runs twice. First es2gears with a 440 Hz tone for 20 s,
encoded with NVENC (H.264 4:4:4 here) and decoded in software, with every
decoded picture checked against FFmpeg's decode of the server's whole
stream (`--out`): a frame decoded against a missing or wrong reference
doesn't match. Then 20 lines typed through the client window into a
terminal in the session, compared with what arrived: a lost key press is
a missing letter, a stuck key repeats.

## The matrix

`tools/m4/matrix.sh`, 20 s per condition, the first 7 s of each run left
out of the video and audio figures (startup, before loss is measured).
*Lost* is frames lost after FEC and NACK, with the RFIs asked for them;
*FEC* and *NACK* are frames they saved; *not exact* is decoded pictures
that differ from the reference decode; latency is glass-to-glass, median;
audio is capture to speaker, median, with frames concealed and how many
of them arrived late rather than not at all.

| condition | netem | lost (RFI) | FEC | NACK | not exact | latency ms | fps | audio concealed (late) | audio ms | typed wrong |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| clean | | 0 (0) | 0 | 0 | 0/1436 | 18.7 | 60 | 7 (7) | 14.6 | 0/20 |
| loss1 | `loss 1%` | 0 (0) | 32 | 0 | 0/1435 | 18.5 | 60 | 0 (0) | 19.5 | 0/20 |
| loss5 | `loss 5%` | 0 (0) | 128 | 0 | 0/1421 | 18.5 | 60 | 0 (0) | 24.4 | 0/20 |
| loss10 | `loss 10%` | 0 (0) | 233 | 0 | 0/1376 | 16.2 | 60 | 1 (1) | 47.6 | 0/20 |
| loss20 | `loss 20%` | 12 (79) | 99 | 1 | 0/428 | 137.3 | 12 | 10 (10) | 37.6 | 1/20 |
| bursty | `loss gemodel 1% 30% 100% 0%` | 0 (0) | 43 | 11 | 0/1424 | 16.2 | 61 | 15 (1) | 29.5 | 0/20 |
| wan | `delay 10ms 2ms rate 1gbit` | 0 (0) | 0 | 0 | 0/1352 | 28.6 | 56 | 0 (0) | 25.0 | 0/20 |
| wan-loss5 | the same, `loss 5%` | 0 (0) | 127 | 0 | 0/1253 | 27.4 | 52 | 3 (3) | 33.6 | 0/20 |
| wan-bursty | the same, bursty | 16 (14) | 29 | 0 | 0/1371 | 29.3 | 58 | 8 (0) | 41.0 | 0/20 |
| far-loss5 | `delay 40ms 5ms rate 1gbit loss 5%` | 0 (0) | 124 | 0 | 0/1279 | 60.8 | 56 | 0 (0) | 68.2 | 0/20 |
| 8mbit | `rate 8mbit` | 0 (0) | 0 | 1 | 0/1395 | 22.1 | 58 | 5 (5) | 16.5 | 0/20 |
| 1mbit | `rate 1mbit` | 6 (16) | 0 | 0 | 0/475 | 129.9 | 22 | 12 (12) | 34.5 | 0/20 |

- **No artifact spreading:** not one picture differs, under any
  condition. As a control, decoding every frame regardless of its
  references gives hundreds of decoder errors through VA-API's HEVC, and
  none through NVENC's H.264, which allows gaps in frame numbers: only
  the comparison of decoded pictures catches it there.
- **No stuck keys:** 20 of 20 lines exact up to 10%. At 20% one line
  came out split; not investigated.
- **Audio at 5% random loss:** nothing concealed. What is concealed
  elsewhere arrived late, not never: stalls of the unprivileged sound
  servers on this laptop (as in M3), and on the clean link too. Under
  bursty loss a burst can outlast the repeats (8 of 8 frames lost
  outright in *wan-bursty*). The buffer grows with the repeats it needs:
  15 ms clean, 24 at 5%, 48 at 10%.
- **At 20% loss** the average loss is over the 20% at which it counts as
  congestion, and the rate sits at its floor, 0.5 Mbit/s: 12 fps.
- **At 1 Mbit/s** the stream starts at 20 Mbit/s and floods the link
  for the first second or so (RTT 450 ms); after that video settles at
  10 to 20 fps with nothing lost and audio goes to 10 ms frames.
- **RFI against keyframes**, *wan-bursty*: NVENC answered 26 RFIs with
  frames predicted from the last good one, about 5.5 KB each against
  10.4 KB for a keyframe; VA-API sent 32 keyframes. Both decoded every
  picture exactly.
- **FEC's cost**, from the server's `video sent` lines (headers, padding
  and parity over the frames' bytes): 4–6% on a clean link; at 5% loss
  120% for es2gears, whose frames are one or two shards and get two
  parity shards each (1.3 Mbit/s more), and 43% for full-motion video
  (ffplay `testsrc2`, 14 Mbit/s). Parity takes at most 0.2 ms a frame.
- **Congestion control**, full-motion video behind `rate 8mbit` (netem's
  default queue, a second of it): the rate settles at 6–10 Mbit/s with
  round trips of 1–3 ms at the median and nothing lost; the QP offset
  sits at +7 to +9, and frames skipped for the backlog, not dropped.
- **M3 still holds:** `tools/m3/session.sh` (takeover, view-only, a
  12 s outage, a desktop crash and a clean exit) as before.

## Findings

- **`tc netem` drops a UDP segmentation offload batch whole**, where a
  real link loses its packets one by one: whole frames vanished at 1%
  loss. Tests turn GSO off (`FARSIGHT_NO_GSO`); farsight keeps it.
- **QUIC packs small datagrams into one packet**, so a small frame's data
  and parity shards were lost together. Shards of a frame of more than
  one are padded past half a datagram.
- **Path MTU discovery's black-hole detection fires on random loss**
  (once in 20 s at 5%), and quinn then drops every queued datagram too
  large for the minimum MTU. The MTU is fixed at 1200; the scheduler no
  longer stops on a datagram that is too large.
- **Delayed acks distort round trips**: the ack-frequency extension asks
  for acks within 2 ms, and each interval's shortest sample is used.
- **netem's jitter reorders packets**, which quinn counts as loss (21–25%
  measured for 5%); with that, the loss rule cut the rate to 2 Mbit/s.
  Jitter in the matrix comes with a rate, which keeps order, and the rate
  decision uses a plain average of loss.
- **Dropping frames after encoding sets off keyframe storms**: each drop
  is a lost reference, each keyframe drops what is queued. The pipeline
  skips frames before encoding instead, while the network is behind.
- **A client asked for another keyframe for frames lost just before the
  one it got.**
- **On a slow path, repair made things worse**: the client asked again
  for frames the server's scheduler had dropped for want of room, and got
  them; frames still arriving 14 ms a shard apart looked stalled and were
  given up; each repeated RFI encoded a frame past the backlog. Fixed by
  ignoring NACKs for dropped frames, scaling "stalled" with the shard gap
  and NACKing only when a repair can arrive within a frame, and dropping
  repeated RFIs on the main thread.
- **NVENC's RFI works on Maxwell** with one reference per frame and a DPB
  of 8, though it lacks "multiple reference frames".
- **Audio "loss" was mostly lateness**: concealed frames that arrived
  after all, from stalls, not from the network.
- **A tap lost just before a pause was never typed** (§4): fixed by
  snapshots that carry the recent events.

## Left for later

- **A run between two machines**, wired and over Wi-Fi.
- **Audio under bursty loss**: the repeats follow the loss rate, not the
  length of bursts. The client could report the bursts it sees.
- **FEC for tiles**, and interleaving for bursty loss.
- **The start on a slow path**: 20 Mbit/s floods a 1 Mbit/s link for a
  second.
- **RFI through VA-API**, which FFmpeg doesn't expose; or intra refresh
  rather than keyframes.
- **Sharing a link**: fairness with other flows, and ECN/L4S, untried.
