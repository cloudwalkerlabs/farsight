//! Frames to datagrams and back (`docs/design.md` §2, "Packetization and
//! loss"). Each frame is split into equal shards and protected by
//! Reed-Solomon parity sized from the measured loss ([`parity`]): any
//! `data` of its shards rebuild it. A frame that still can't be rebuilt is
//! lost, and the client asks for a keyframe.

use bytes::Bytes;
use farsight_proto::datagram::{self, VIDEO_OVERHEAD};
use farsight_proto::video::{FLAG_KEYFRAME, FragmentHeader, shard_size};

/// Partial frames still waiting for fragments. Older ones are given up.
const MAX_PARTIAL: usize = 8;

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
    pub capture_us: u64,
    pub encode_us: u32,
}

/// Server side: numbers frames and splits them into datagrams.
#[derive(Debug, Default)]
pub struct Packetizer {
    next_frame: u32,
}

impl Packetizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The datagrams for `frame`, each at most `max_datagram` bytes: its
    /// data shards in order, then parity for packet loss rate `loss`.
    pub fn packetize(&mut self, frame: &EncodedFrame, max_datagram: usize, loss: f64) -> Vec<Bytes> {
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
        let number = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);
        let mut header = FragmentHeader {
            flags: if frame.keyframe { FLAG_KEYFRAME } else { 0 },
            epoch: frame.epoch,
            frame: number,
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

/// Client side: collects fragments into frames. Frames are delivered in
/// order and only when whole, rebuilt from parity if need be; anything
/// older than a delivered frame is dropped, and gaps are counted as losses.
#[derive(Debug)]
pub struct Reassembler {
    /// Oldest first. Short, so scans are cheap.
    partial: Vec<Partial>,
    /// The newest frame accounted for, delivered or given up. Anything up
    /// to it is stale.
    mark: Option<u32>,
    timeout_us: u64,
    lost: u32,
    recovered: u32,
}

#[derive(Debug)]
struct Partial {
    header: FragmentHeader,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    first_us: u64,
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
}

/// `a` is before `b`, across wrap-around.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

impl Reassembler {
    /// A partial frame is given up `timeout_us` after its first fragment.
    pub fn new(timeout_us: u64) -> Self {
        Self { partial: Vec::new(), mark: None, timeout_us, lost: 0, recovered: 0 }
    }

    pub fn push(&mut self, header: FragmentHeader, payload: &[u8], now_us: u64) -> Option<Frame> {
        if self.mark.is_some_and(|mark| !before(mark, header.frame)) {
            return None;
        }
        let i = match self.partial.iter().position(|p| !before(p.header.frame, header.frame)) {
            Some(i) if self.partial[i].header.frame == header.frame => i,
            at => {
                let i = at.unwrap_or(self.partial.len());
                let partial =
                    Partial { header, shards: vec![None; header.count as usize], have: 0, first_us: now_us };
                self.partial.insert(i, partial);
                i
            }
        };
        let partial = &mut self.partial[i];
        let h = &partial.header;
        let size = partial.shards.iter().flatten().next().map_or(payload.len(), Vec::len);
        if (h.count, h.data, h.len, size) != (header.count, header.data, header.len, payload.len()) {
            return None; // corrupt; let it time out
        }
        let slot = &mut partial.shards[header.index as usize];
        if slot.is_some() {
            return None;
        }
        *slot = Some(payload.to_vec());
        partial.have += 1;
        if partial.have < header.data as usize {
            if self.partial.len() > MAX_PARTIAL {
                self.give_up_through(0);
            }
            return None;
        }
        // Everything older is superseded.
        let partial = self.partial.drain(..=i).next_back().unwrap();
        self.account(header.frame, i as u32);
        let (header, first_us) = (partial.header, partial.first_us);
        let rebuilt = partial.shards[..header.data as usize].iter().any(Option::is_none);
        match partial.rebuild() {
            Ok(data) => {
                self.recovered += rebuilt as u32;
                Some(Frame { header, data, first_us, complete_us: now_us })
            }
            Err(err) => {
                tracing::debug!(frame = header.frame, %err, "rebuilding a frame");
                self.lost += 1;
                None
            }
        }
    }

    /// Gives up partial frames that have waited too long, and any older.
    pub fn expire(&mut self, now_us: u64) {
        let timeout = self.timeout_us;
        if let Some(i) = self.partial.iter().rposition(|p| now_us.saturating_sub(p.first_us) >= timeout) {
            self.give_up_through(i);
        }
    }

    /// Frames lost since the last call.
    pub fn take_lost(&mut self) -> u32 {
        std::mem::take(&mut self.lost)
    }

    /// Frames rebuilt from parity since the last call.
    pub fn take_recovered(&mut self) -> u32 {
        std::mem::take(&mut self.recovered)
    }

    fn give_up_through(&mut self, i: usize) {
        let frame = self.partial[i].header.frame;
        self.partial.drain(..=i);
        self.account(frame, i as u32);
        self.lost += 1;
    }

    /// Moves the mark to `frame`, counting the frames skipped on the way.
    /// With no mark yet, only the `dropped` partials are known to be lost.
    fn account(&mut self, frame: u32, dropped: u32) {
        let lost = match self.mark {
            Some(mark) => frame.wrapping_sub(mark).wrapping_sub(1),
            None => dropped,
        };
        if lost > 0 {
            tracing::debug!(before = frame, lost, "frames lost");
        }
        self.lost += lost;
        self.mark = Some(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use farsight_proto::datagram::Datagram;

    fn frame(data: &[u8], keyframe: bool) -> EncodedFrame<'_> {
        EncodedFrame { data, keyframe, epoch: 1, capture_us: 10, encode_us: 2 }
    }

    fn split(d: &Bytes) -> (FragmentHeader, Vec<u8>) {
        match Datagram::decode(d).unwrap() {
            Datagram::Video(h, p) => (h, p.to_vec()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn splits_to_the_datagram_size() {
        let data: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let dgrams = Packetizer::new().packetize(&frame(&data, true), 1200, 0.0);
        // Three data shards, and a keyframe gets parity even on a clean link.
        assert_eq!(split(&dgrams[0]).0.data, 3);
        assert_eq!(dgrams.len(), 4);
        assert!(dgrams.iter().all(|d| d.len() <= 1200));
        let mut rx = Reassembler::new(50_000);
        let mut out = None;
        // Out of order is fine, and any three of the four will do.
        for d in dgrams.iter().rev() {
            let (h, p) = split(d);
            out = out.or(rx.push(h, &p, 5));
        }
        let out = out.unwrap();
        assert_eq!(out.data, data);
        assert!(out.header.keyframe());
        assert_eq!(rx.take_lost(), 0);
    }

    #[test]
    fn empty_frame_is_one_fragment() {
        let dgrams = Packetizer::new().packetize(&frame(&[], false), 1200, 0.0);
        assert_eq!(dgrams.len(), 1);
        let (h, p) = split(&dgrams[0]);
        assert_eq!(Reassembler::new(1).push(h, &p, 0).unwrap().data, b"");
    }

    #[test]
    fn incomplete_frame_is_superseded_and_counted() {
        let mut tx = Packetizer::new();
        let mut rx = Reassembler::new(50_000);
        let f0 = tx.packetize(&frame(&[0; 3000], true), 1200, 0.0);
        let f1 = tx.packetize(&frame(&[1; 100], false), 1200, 0.0);
        let (h, p) = split(&f0[0]);
        assert!(rx.push(h, &p, 0).is_none());
        let (h, p) = split(&f1[0]);
        assert_eq!(rx.push(h, &p, 1).unwrap().header.frame, 1);
        assert_eq!(rx.take_lost(), 1);
        // The rest of frame 0 arriving late is ignored.
        let (h, p) = split(&f0[1]);
        assert!(rx.push(h, &p, 2).is_none());
        assert_eq!(rx.take_lost(), 0);
    }

    #[test]
    fn wholly_missing_frames_are_counted() {
        let mut tx = Packetizer::new();
        let mut rx = Reassembler::new(50_000);
        let all: Vec<_> = (0..4).map(|_| tx.packetize(&frame(&[7; 10], false), 1200, 0.0)).collect();
        for i in [0, 3] {
            let (h, p) = split(&all[i][0]);
            assert!(rx.push(h, &p, 0).is_some());
        }
        assert_eq!(rx.take_lost(), 2);
    }

    #[test]
    fn partial_frames_time_out() {
        let mut tx = Packetizer::new();
        let mut rx = Reassembler::new(50_000);
        let f = tx.packetize(&frame(&[0; 3000], true), 1200, 0.0);
        let (h, p) = split(&f[0]);
        rx.push(h, &p, 0);
        rx.expire(10_000);
        assert_eq!(rx.take_lost(), 0);
        rx.expire(60_000);
        assert_eq!(rx.take_lost(), 1);
        // Its late fragments are stale, and the next frame isn't a gap.
        let (h, p) = split(&f[1]);
        assert!(rx.push(h, &p, 60_001).is_none());
        let (h, p) = split(&tx.packetize(&frame(&[1], false), 1200, 0.0)[0]);
        assert!(rx.push(h, &p, 60_002).is_some());
        assert_eq!(rx.take_lost(), 0);
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
        let dgrams = Packetizer::new().packetize(&frame(&data, false), 1200, 0.05);
        let (h, _) = split(&dgrams[0]);
        let parity = (h.count - h.data) as usize;
        assert!(parity >= 2);
        let mut rx = Reassembler::new(50_000);
        let mut out = None;
        // Lose the short last data shard and as many more as there is parity.
        let lost: Vec<usize> = (0..parity - 1).chain([h.data as usize - 1]).collect();
        for (i, d) in dgrams.iter().enumerate().filter(|(i, _)| !lost.contains(i)) {
            let (h, p) = split(d);
            if let Some(f) = rx.push(h, &p, i as u64) {
                out = Some(f);
            }
        }
        assert_eq!(out.unwrap().data, data);
        assert_eq!((rx.take_recovered(), rx.take_lost()), (1, 0));
    }

    #[test]
    fn shards_of_a_small_frame_never_share_a_packet() {
        let dgrams = Packetizer::new().packetize(&frame(&[3; 200], false), 1200, 0.05);
        assert!(dgrams.len() >= 3);
        // QUIC would pack datagrams this small together; padded, no two fit.
        assert!(dgrams.iter().all(|d| 2 * d.len() > 1200), "{:?}", dgrams.iter().map(Bytes::len).collect::<Vec<_>>());
        let mut rx = Reassembler::new(50_000);
        let (h, p) = split(dgrams.last().unwrap());
        assert_eq!(rx.push(h, &p, 0).unwrap().data, [3; 200]);
        // Alone, a frame isn't padded.
        let dgrams = Packetizer::new().packetize(&frame(&[3; 200], false), 1200, 0.0);
        assert_eq!(dgrams.len(), 1);
        assert_eq!(dgrams[0].len(), VIDEO_OVERHEAD + 200);
    }

    #[test]
    fn too_much_loss_loses_the_frame() {
        let mut tx = Packetizer::new();
        let dgrams = tx.packetize(&frame(&[9; 5000], false), 1200, 0.05);
        let (h, _) = split(&dgrams[0]);
        let mut rx = Reassembler::new(50_000);
        for d in &dgrams[(h.count - h.data + 1) as usize..] {
            let (h, p) = split(d);
            assert!(rx.push(h, &p, 0).is_none());
        }
        rx.expire(50_000);
        assert_eq!(rx.take_lost(), 1);
    }

    #[test]
    fn frame_numbers_wrap() {
        let mut tx = Packetizer { next_frame: u32::MAX };
        let mut rx = Reassembler::new(50_000);
        for expect in [u32::MAX, 0, 1] {
            let (h, p) = split(&tx.packetize(&frame(&[1], false), 1200, 0.0)[0]);
            assert_eq!(rx.push(h, &p, 0).unwrap().header.frame, expect);
        }
        assert_eq!(rx.take_lost(), 0);
    }
}
