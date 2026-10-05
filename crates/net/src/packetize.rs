//! Frames to datagrams and back (`docs/design.md` §2, "Packetization and
//! loss").
//!
//! Each frame is split into equal shards and protected by Reed-Solomon
//! parity sized from the measured loss ([`parity`]): any `data` of its
//! shards rebuild it. On a path quicker than a frame, the client also asks
//! for what is still missing (a NACK) and the server sends it again. A
//! frame that can be neither rebuilt nor repaired is lost, and the client
//! asks for reference frame invalidation.

use bytes::Bytes;
use farsight_proto::datagram::{self, Nack, VIDEO_OVERHEAD};
use farsight_proto::video::{FLAG_KEYFRAME, FragmentHeader, before, shard_size};
use std::collections::VecDeque;

/// Frames tracked at once, from the oldest not yet delivered. Older ones
/// are given up.
const MAX_SLOTS: usize = 64;

/// How often a frame may still be lost after FEC: ordinary frames, and
/// keyframes, which cost far more to send again.
const FRAME_FAILURE: f64 = 0.002;
const KEYFRAME_FAILURE: f64 = 0.0005;

/// Loss comes in bursts more than independent loss would; parity is sized
/// for this much more of it.
const BURST_MARGIN: f64 = 1.5;

/// Parity is sized for at least this loss, so a frame survives the odd
/// loss that the measurement hasn't caught.
const MIN_LOSS: f64 = 0.0005;

/// The newest frame counts as stalled this long after its last shard, if
/// no later frame shows that it was all sent.
const STALL_US: u64 = 10_000;

/// A frame is asked for again at most this many times.
const MAX_NACKS: u8 = 2;

/// How many parity shards protect `data` data shards at packet loss rate
/// `loss` (0–1): the fewest that leave the frame unrecoverable no more
/// often than [`FRAME_FAILURE`] (or [`KEYFRAME_FAILURE`]), taking losses as
/// independent at [`BURST_MARGIN`] times the rate.
pub fn parity(data: usize, loss: f64, keyframe: bool) -> usize {
    let p = (loss * BURST_MARGIN).clamp(MIN_LOSS, 0.5);
    let target = if keyframe { KEYFRAME_FAILURE } else { FRAME_FAILURE };
    let max = (2 * data + 2).min(u16::MAX as usize - data);
    (0..max).find(|&m| beyond(data + m, m, p) <= target).unwrap_or(max)
}

/// P(more than `m` of `n` packets are lost), each with probability `p`.
fn beyond(n: usize, m: usize, p: f64) -> f64 {
    // The binomial terms P(X = i), from (1 - p)^n upwards.
    let mut term = (1.0 - p).powi(n as i32);
    let mut at_most = term;
    for i in 0..m {
        term *= (n - i) as f64 / (i + 1) as f64 * p / (1.0 - p);
        at_most += term;
    }
    (1.0 - at_most).max(0.0)
}

/// An encoded frame, as the encoder produced it.
#[derive(Debug, Clone, Copy)]
pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    pub epoch: u16,
    pub frame: u32,
    /// The newest frame it references ([`FragmentHeader::refs`]).
    pub refs: u32,
    pub capture_us: u64,
    pub encode_us: u32,
}

/// The datagrams for `frame`, each at most `max_datagram` bytes: its data
/// shards in order, then parity for packet loss rate `loss`.
pub fn packetize(frame: &EncodedFrame, max_datagram: usize, loss: f64) -> Vec<Bytes> {
    let len = frame.data.len();
    let chunk = (max_datagram.saturating_sub(VIDEO_OVERHEAD) & !1).max(2);
    let data = len.div_ceil(chunk).max(1);
    assert!(data < u16::MAX as usize && len <= u32::MAX as usize, "frame of {len} bytes is too large");
    let parity = parity(data, loss, frame.keyframe);
    // Past half a datagram, no two shards share a packet (and a loss).
    let size = match data + parity {
        1 => shard_size(len, data),
        _ => shard_size(len, data).max((chunk / 2 + 2) & !1).min(chunk),
    };
    let mut header = FragmentHeader {
        flags: if frame.keyframe { FLAG_KEYFRAME } else { 0 },
        epoch: frame.epoch,
        frame: frame.frame,
        refs: frame.refs,
        index: 0,
        count: (data + parity) as u16,
        data: data as u16,
        len: len as u32,
        capture_us: frame.capture_us,
        encode_us: frame.encode_us,
    };
    // Every shard is `size` long; the frame ends with padding.
    let shards: Vec<Vec<u8>> = (0..data)
        .map(|i| {
            let mut s = frame.data[(i * size).min(len)..((i + 1) * size).min(len)].to_vec();
            s.resize(size, 0);
            s
        })
        .collect();
    let recovery = if parity == 0 {
        Vec::new()
    } else {
        reed_solomon_simd::encode(data, parity, &shards).expect("shard counts and sizes are in range")
    };
    shards
        .iter()
        .chain(&recovery)
        .enumerate()
        .map(|(i, payload)| {
            header.index = i as u16;
            let mut out = Vec::with_capacity(VIDEO_OVERHEAD + payload.len());
            datagram::encode_video(&header, payload, &mut out);
            Bytes::from(out)
        })
        .collect()
}

/// A whole frame, put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The header of its first fragment to arrive (index aside, every
    /// fragment carries the same one).
    pub header: FragmentHeader,
    pub data: Vec<u8>,
    /// When its first and last fragments arrived, in the caller's µs.
    pub first_us: u64,
    pub complete_us: u64,
}

/// Whether, and how patiently, to ask for missing shards again.
#[derive(Debug, Clone, Copy)]
pub struct Repair {
    /// The path is quick enough for a NACK to beat giving up.
    pub nack: bool,
    /// How long to wait for an answer before asking again, or giving up.
    pub wait_us: u64,
}

impl Repair {
    pub const NONE: Repair = Repair { nack: false, wait_us: 0 };
}

/// Client side: collects shards into frames, rebuilds them from parity,
/// and hands them over in order and only when whole. A frame still missing
/// shards holds the ones after it back while it can still be repaired; once
/// it can't, it is lost.
#[derive(Debug)]
pub struct Reassembler {
    /// One per frame number after `mark`, oldest first.
    slots: VecDeque<Slot>,
    /// The newest frame accounted for, delivered or given up. Anything up
    /// to it is stale.
    mark: Option<u32>,
    timeout_us: u64,
    lost: Option<Lost>,
    recovered: u32,
    repaired: u32,
}

/// Frames given up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lost {
    pub count: u32,
    pub newest: u32,
}

#[derive(Debug)]
struct Slot {
    frame: u32,
    state: State,
    /// When the first and last of its shards arrived, or when a later
    /// frame showed it missing.
    first_us: u64,
    last_us: u64,
    nacked_us: Option<u64>,
    nacks: u8,
}

#[derive(Debug)]
enum State {
    /// Not one shard yet.
    Missing,
    Partial(Partial),
    Ready(Frame),
    /// Couldn't be rebuilt.
    Dead,
}

#[derive(Debug)]
struct Partial {
    header: FragmentHeader,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
}

impl Partial {
    /// The frame's bytes, once `data` shards are in.
    fn rebuild(self) -> Result<Vec<u8>, reed_solomon_simd::Error> {
        let (data, len) = (self.header.data as usize, self.header.len as usize);
        let mut shards = self.shards;
        if shards[..data].iter().any(Option::is_none) {
            let parity = shards.len() - data;
            let original = shards[..data].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
            let recovery = shards[data..].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
            for (i, shard) in reed_solomon_simd::decode(data, parity, original, recovery)? {
                shards[i] = Some(shard);
            }
        }
        let mut out = Vec::with_capacity(len);
        for shard in shards.into_iter().take(data) {
            out.extend_from_slice(&shard.expect("every data shard is in or rebuilt"));
        }
        out.truncate(len);
        Ok(out)
    }

    /// Shards to ask for: as many as are needed, data first.
    fn wanted(&self) -> Vec<u16> {
        let need = self.header.data as usize - self.have;
        let missing = self.shards.iter().enumerate().filter(|(_, s)| s.is_none()).map(|(i, _)| i as u16);
        missing.take(need).collect()
    }
}

impl Reassembler {
    /// A frame is given up `timeout_us` after its first fragment.
    pub fn new(timeout_us: u64) -> Self {
        Self { slots: VecDeque::new(), mark: None, timeout_us, lost: None, recovered: 0, repaired: 0 }
    }

    /// Frames are waiting: [`Reassembler::poll`] has something to decide.
    pub fn pending(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Takes one shard; [`Reassembler::poll`] hands over what it completes.
    pub fn push(&mut self, header: FragmentHeader, payload: &[u8], now_us: u64) {
        let mark = *self.mark.get_or_insert(header.frame.wrapping_sub(1));
        if !before(mark, header.frame) {
            return;
        }
        // A slot for every frame up to this one: those in between are gaps.
        while self.newest().is_none_or(|n| before(n, header.frame)) {
            let frame = self.newest().unwrap_or(mark).wrapping_add(1);
            self.slots.push_back(Slot {
                frame,
                state: State::Missing,
                first_us: now_us,
                last_us: now_us,
                nacked_us: None,
                nacks: 0,
            });
        }
        while self.slots.len() > MAX_SLOTS {
            self.give_up_front();
        }
        let Some(slot) = self.slots.iter_mut().find(|s| s.frame == header.frame) else { return };
        if let State::Missing = slot.state {
            slot.state = State::Partial(Partial { header, shards: vec![None; header.count as usize], have: 0 });
            slot.first_us = now_us;
        }
        let State::Partial(p) = &mut slot.state else { return };
        let h = &p.header;
        let size = p.shards.iter().flatten().next().map_or(payload.len(), Vec::len);
        if (h.count, h.data, h.len, size) != (header.count, header.data, header.len, payload.len()) {
            return; // corrupt; let it time out
        }
        let shard = &mut p.shards[header.index as usize];
        if shard.is_some() {
            return;
        }
        *shard = Some(payload.to_vec());
        p.have += 1;
        slot.last_us = now_us;
        if p.have < header.data as usize {
            return;
        }
        let State::Partial(p) = std::mem::replace(&mut slot.state, State::Dead) else { unreachable!() };
        let header = p.header;
        let rebuilt = p.shards[..header.data as usize].iter().any(Option::is_none);
        match p.rebuild() {
            Ok(data) => {
                self.recovered += rebuilt as u32;
                self.repaired += (slot.nacks > 0) as u32;
                slot.state = State::Ready(Frame { header, data, first_us: slot.first_us, complete_us: now_us });
            }
            Err(err) => tracing::debug!(frame = header.frame, %err, "rebuilding a frame"),
        }
    }

    /// The frames ready to decode, in order, and the shards to ask for
    /// again.
    pub fn poll(&mut self, now_us: u64, repair: Repair) -> (Vec<Frame>, Vec<Nack>) {
        let mut nacks = Vec::new();
        if repair.nack {
            let newest = self.newest();
            for slot in &mut self.slots {
                let stalled = Some(slot.frame) != newest || now_us.saturating_sub(slot.last_us) >= STALL_US;
                let due = slot.nacked_us.is_none_or(|t| now_us.saturating_sub(t) >= repair.wait_us);
                let shards = match &slot.state {
                    State::Missing => Vec::new(),
                    State::Partial(p) => p.wanted(),
                    State::Ready(_) | State::Dead => continue,
                };
                if stalled && due && slot.nacks < MAX_NACKS {
                    nacks.push(Nack { frame: slot.frame, shards });
                    slot.nacked_us = Some(now_us);
                    slot.nacks += 1;
                }
            }
        }
        let mut out = Vec::new();
        while let Some(front) = self.slots.front() {
            let give_up = match &front.state {
                State::Ready(_) => {
                    let Some(Slot { frame, state: State::Ready(f), .. }) = self.slots.pop_front() else {
                        unreachable!()
                    };
                    self.mark = Some(frame);
                    out.push(f);
                    continue;
                }
                State::Dead => true,
                State::Missing | State::Partial(_) => {
                    // A later frame that is whole: any, or a keyframe.
                    let later_ready = |keyframe: bool| {
                        self.slots
                            .iter()
                            .skip(1)
                            .any(|s| matches!(&s.state, State::Ready(f) if f.header.keyframe() || !keyframe))
                    };
                    let exhausted = front.nacks >= MAX_NACKS
                        && front.nacked_us.is_some_and(|t| now_us.saturating_sub(t) >= repair.wait_us);
                    now_us.saturating_sub(front.first_us) >= self.timeout_us
                        || later_ready(true)
                        || (!repair.nack && later_ready(false))
                        || (repair.nack && exhausted)
                }
            };
            if !give_up {
                break;
            }
            self.give_up_front();
        }
        (out, nacks)
    }

    /// Frames lost since the last call.
    pub fn take_lost(&mut self) -> Option<Lost> {
        self.lost.take()
    }

    /// Frames rebuilt from parity since the last call.
    pub fn take_recovered(&mut self) -> u32 {
        std::mem::take(&mut self.recovered)
    }

    /// Frames completed by shards sent again since the last call.
    pub fn take_repaired(&mut self) -> u32 {
        std::mem::take(&mut self.repaired)
    }

    fn newest(&self) -> Option<u32> {
        self.slots.back().map(|s| s.frame)
    }

    fn give_up_front(&mut self) {
        if let Some(slot) = self.slots.pop_front() {
            tracing::debug!(frame = slot.frame, nacks = slot.nacks, "frame lost");
            self.mark = Some(slot.frame);
            let count = self.lost.map_or(0, |l| l.count) + 1;
            self.lost = Some(Lost { count, newest: slot.frame });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use farsight_proto::datagram::Datagram;

    fn frame(data: &[u8], keyframe: bool, n: u32) -> EncodedFrame<'_> {
        EncodedFrame { data, keyframe, epoch: 1, frame: n, refs: n.wrapping_sub(1), capture_us: 10, encode_us: 2 }
    }

    fn split(d: &Bytes) -> (FragmentHeader, Vec<u8>) {
        match Datagram::decode(d).unwrap() {
            Datagram::Video(h, p) => (h, p.to_vec()),
            other => panic!("{other:?}"),
        }
    }

    /// Pushes `dgrams` at `now` and polls without repair.
    fn feed(rx: &mut Reassembler, dgrams: &[Bytes], now: u64) -> Vec<Frame> {
        push(rx, dgrams, now);
        rx.poll(now, Repair::NONE).0
    }

    fn push(rx: &mut Reassembler, dgrams: &[Bytes], now: u64) {
        for d in dgrams {
            let (h, p) = split(d);
            rx.push(h, &p, now);
        }
    }

    fn lost(rx: &mut Reassembler) -> u32 {
        rx.take_lost().map_or(0, |l| l.count)
    }

    fn numbers(frames: &[Frame]) -> Vec<u32> {
        frames.iter().map(|f| f.header.frame).collect()
    }

    #[test]
    fn splits_to_the_datagram_size() {
        let data: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let dgrams = packetize(&frame(&data, true, 0), 1200, 0.0);
        // Three data shards, and a keyframe gets parity even on a clean link.
        assert_eq!(split(&dgrams[0]).0.data, 3);
        assert_eq!(dgrams.len(), 4);
        assert!(dgrams.iter().all(|d| d.len() <= 1200));
        let mut rx = Reassembler::new(50_000);
        // Out of order is fine, and any three of the four will do.
        let out = feed(&mut rx, &[dgrams[3].clone(), dgrams[2].clone(), dgrams[1].clone()], 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, data);
        assert!(out[0].header.keyframe());
        assert!(feed(&mut rx, &dgrams[..1], 6).is_empty());
        assert_eq!((lost(&mut rx), rx.take_recovered()), (0, 1));
    }

    #[test]
    fn empty_frame_is_one_fragment() {
        let dgrams = packetize(&frame(&[], false, 0), 1200, 0.0);
        assert_eq!(dgrams.len(), 1);
        assert_eq!(feed(&mut Reassembler::new(1), &dgrams, 0)[0].data, b"");
    }

    #[test]
    fn parity_follows_loss() {
        // A clean link: small frames go bare, larger ones get a shard.
        assert_eq!(parity(1, 0.0, false), 0);
        assert_eq!(parity(10, 0.0, false), 1);
        let p5 = parity(10, 0.05, false);
        assert!((3..=6).contains(&p5), "{p5}");
        assert!(parity(100, 0.05, true) > parity(100, 0.05, false));
        assert!(parity(250, 0.05, true) < 60);
        // Never more than twice the data, plus two.
        assert_eq!(parity(1, 0.5, true), 4);
    }

    #[test]
    fn lost_shards_are_rebuilt() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
        let dgrams = packetize(&frame(&data, false, 0), 1200, 0.05);
        let (h, _) = split(&dgrams[0]);
        let parity = (h.count - h.data) as usize;
        assert!(parity >= 2);
        // Lose the padded last data shard and as many more as there is parity.
        let dropped: Vec<usize> = (0..parity - 1).chain([h.data as usize - 1]).collect();
        let kept: Vec<Bytes> =
            dgrams.iter().enumerate().filter(|(i, _)| !dropped.contains(i)).map(|(_, d)| d.clone()).collect();
        let mut rx = Reassembler::new(50_000);
        assert_eq!(feed(&mut rx, &kept, 0)[0].data, data);
        assert_eq!((rx.take_recovered(), lost(&mut rx)), (1, 0));
    }

    #[test]
    fn shards_of_a_small_frame_never_share_a_packet() {
        let dgrams = packetize(&frame(&[3; 200], false, 0), 1200, 0.05);
        assert!(dgrams.len() >= 3);
        // QUIC would pack datagrams this small together; padded, no two fit.
        assert!(dgrams.iter().all(|d| 2 * d.len() > 1200), "{:?}", dgrams.iter().map(Bytes::len).collect::<Vec<_>>());
        let mut rx = Reassembler::new(50_000);
        assert_eq!(feed(&mut rx, &dgrams[dgrams.len() - 1..], 0)[0].data, [3; 200]);
        // Alone, a frame isn't padded.
        let dgrams = packetize(&frame(&[3; 200], false, 0), 1200, 0.0);
        assert_eq!(dgrams.len(), 1);
        assert_eq!(dgrams[0].len(), VIDEO_OVERHEAD + 200);
    }

    #[test]
    fn without_repair_a_later_frame_supersedes() {
        let mut rx = Reassembler::new(50_000);
        let f0 = packetize(&frame(&[0; 3000], false, 0), 1200, 0.0);
        let f1 = packetize(&frame(&[1; 100], false, 1), 1200, 0.0);
        assert!(feed(&mut rx, &f0[..1], 0).is_empty());
        assert_eq!(numbers(&feed(&mut rx, &f1, 1)), [1]);
        assert_eq!(lost(&mut rx), 1);
        // The rest of frame 0 arriving late is ignored.
        assert!(feed(&mut rx, &f0[1..], 2).is_empty());
        assert_eq!(lost(&mut rx), 0);
    }

    #[test]
    fn wholly_missing_frames_are_counted() {
        let mut rx = Reassembler::new(50_000);
        let all: Vec<_> = (0..4).map(|n| packetize(&frame(&[7; 10], false, n), 1200, 0.0)).collect();
        assert_eq!(numbers(&feed(&mut rx, &all[0], 0)), [0]);
        assert_eq!(numbers(&feed(&mut rx, &all[3], 0)), [3]);
        assert_eq!(rx.take_lost(), Some(Lost { count: 2, newest: 2 }));
    }

    #[test]
    fn partial_frames_time_out() {
        let mut rx = Reassembler::new(50_000);
        let f = packetize(&frame(&[0; 3000], true, 0), 1200, 0.0);
        feed(&mut rx, &f[..1], 0);
        rx.poll(10_000, Repair::NONE);
        assert_eq!(lost(&mut rx), 0);
        rx.poll(60_000, Repair::NONE);
        assert_eq!(lost(&mut rx), 1);
        // Its late fragments are stale, and the next frame isn't a gap.
        assert!(feed(&mut rx, &f[1..2], 60_001).is_empty());
        assert_eq!(numbers(&feed(&mut rx, &packetize(&frame(&[1], false, 1), 1200, 0.0), 60_002)), [1]);
        assert_eq!(lost(&mut rx), 0);
    }

    #[test]
    fn frame_numbers_wrap() {
        let mut rx = Reassembler::new(50_000);
        for n in [u32::MAX, 0, 1] {
            assert_eq!(numbers(&feed(&mut rx, &packetize(&frame(&[1], false, n), 1200, 0.0), 0)), [n]);
        }
        assert_eq!(lost(&mut rx), 0);
    }

    const REPAIR: Repair = Repair { nack: true, wait_us: 5_000 };

    #[test]
    fn a_stalled_frame_is_nacked_and_holds_the_next_back() {
        let mut rx = Reassembler::new(250_000);
        let f0 = packetize(&frame(&[0; 3000], false, 0), 1200, 0.0);
        let f1 = packetize(&frame(&[1; 100], false, 1), 1200, 0.0);
        push(&mut rx, &f0[..1], 0);
        // Still arriving: nothing to ask for yet.
        assert!(rx.poll(1_000, REPAIR).1.is_empty());
        push(&mut rx, &f1, 2_000);
        // Frame 1 is whole but waits; frame 0 is asked for.
        let (out, nacks) = rx.poll(2_000, REPAIR);
        assert!(out.is_empty());
        assert_eq!(nacks, [Nack { frame: 0, shards: vec![1, 2] }]);
        // Not again until the answer is due.
        assert!(rx.poll(3_000, REPAIR).1.is_empty());
        push(&mut rx, &f0[1..], 4_000);
        let (out, _) = rx.poll(4_000, REPAIR);
        assert_eq!(numbers(&out), [0, 1]);
        assert_eq!((rx.take_repaired(), lost(&mut rx)), (1, 0));
    }

    #[test]
    fn the_last_frame_is_nacked_once_it_stalls() {
        let mut rx = Reassembler::new(250_000);
        let f0 = packetize(&frame(&[0; 3000], false, 0), 1200, 0.0);
        push(&mut rx, &f0[..2], 0);
        assert!(rx.poll(STALL_US - 1, REPAIR).1.is_empty());
        assert_eq!(rx.poll(STALL_US, REPAIR).1, [Nack { frame: 0, shards: vec![2] }]);
    }

    #[test]
    fn a_missing_frame_is_nacked_whole_then_given_up() {
        let mut rx = Reassembler::new(250_000);
        feed(&mut rx, &packetize(&frame(&[0], false, 0), 1200, 0.0), 0);
        push(&mut rx, &packetize(&frame(&[2], false, 2), 1200, 0.0), 1_000);
        let (out, nacks) = rx.poll(1_000, REPAIR);
        assert!(out.is_empty());
        assert_eq!(nacks, [Nack { frame: 1, shards: vec![] }]);
        assert_eq!(rx.poll(6_000, REPAIR).1.len(), 1, "asked twice");
        let (out, nacks) = rx.poll(11_000, REPAIR);
        assert!(nacks.is_empty());
        assert_eq!(numbers(&out), [2]);
        assert_eq!(lost(&mut rx), 1);
    }

    #[test]
    fn a_keyframe_goes_at_once() {
        let mut rx = Reassembler::new(250_000);
        push(&mut rx, &packetize(&frame(&[0; 3000], false, 0), 1200, 0.0)[..1], 0);
        push(&mut rx, &packetize(&frame(&[1; 10], true, 1), 1200, 0.0), 1);
        assert_eq!(numbers(&rx.poll(1, REPAIR).0), [1]);
        assert_eq!(lost(&mut rx), 1);
    }
}
