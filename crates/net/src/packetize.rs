//! Frames to datagrams and back (`docs/design.md` §2, "Packetization and
//! loss"). FEC and NACK come in M4; for now a frame with a missing fragment
//! is lost, and the client asks for a keyframe.

use bytes::Bytes;
use farsight_proto::datagram::{self, VIDEO_OVERHEAD};
use farsight_proto::video::{FLAG_KEYFRAME, FragmentHeader};

/// Partial frames still waiting for fragments. Older ones are given up.
const MAX_PARTIAL: usize = 8;

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

    /// The datagrams for `frame`, each at most `max_datagram` bytes.
    pub fn packetize(&mut self, frame: &EncodedFrame, max_datagram: usize) -> Vec<Bytes> {
        let chunk = max_datagram.saturating_sub(VIDEO_OVERHEAD).max(1);
        let count = frame.data.len().div_ceil(chunk).max(1);
        assert!(count <= u16::MAX as usize, "frame of {} bytes is too large", frame.data.len());
        let number = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);
        let mut header = FragmentHeader {
            flags: if frame.keyframe { FLAG_KEYFRAME } else { 0 },
            epoch: frame.epoch,
            frame: number,
            index: 0,
            count: count as u16,
            capture_us: frame.capture_us,
            encode_us: frame.encode_us,
        };
        (0..count)
            .map(|i| {
                header.index = i as u16;
                let payload = &frame.data[(i * chunk).min(frame.data.len())..((i + 1) * chunk).min(frame.data.len())];
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
/// order and only when whole; anything older than a delivered frame is
/// dropped, and gaps are counted as losses.
#[derive(Debug)]
pub struct Reassembler {
    /// Oldest first. Short, so scans are cheap.
    partial: Vec<Partial>,
    /// The newest frame accounted for, delivered or given up. Anything up
    /// to it is stale.
    mark: Option<u32>,
    timeout_us: u64,
    lost: u32,
}

#[derive(Debug)]
struct Partial {
    header: FragmentHeader,
    fragments: Vec<Option<Vec<u8>>>,
    missing: usize,
    first_us: u64,
}

/// `a` is before `b`, across wrap-around.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

impl Reassembler {
    /// A partial frame is given up `timeout_us` after its first fragment.
    pub fn new(timeout_us: u64) -> Self {
        Self { partial: Vec::new(), mark: None, timeout_us, lost: 0 }
    }

    pub fn push(&mut self, header: FragmentHeader, payload: &[u8], now_us: u64) -> Option<Frame> {
        if self.mark.is_some_and(|mark| !before(mark, header.frame)) {
            return None;
        }
        let i = match self.partial.iter().position(|p| !before(p.header.frame, header.frame)) {
            Some(i) if self.partial[i].header.frame == header.frame => i,
            at => {
                let i = at.unwrap_or(self.partial.len());
                let partial = Partial {
                    header,
                    fragments: vec![None; header.count as usize],
                    missing: header.count as usize,
                    first_us: now_us,
                };
                self.partial.insert(i, partial);
                i
            }
        };
        let partial = &mut self.partial[i];
        if partial.header.count != header.count {
            return None; // corrupt; let it time out
        }
        let slot = &mut partial.fragments[header.index as usize];
        if slot.is_some() {
            return None;
        }
        *slot = Some(payload.to_vec());
        partial.missing -= 1;
        if partial.missing > 0 {
            if self.partial.len() > MAX_PARTIAL {
                self.give_up_through(0);
            }
            return None;
        }
        // Everything older is superseded.
        let partial = self.partial.drain(..=i).next_back().unwrap();
        self.account(header.frame, i as u32);
        let mut data = Vec::with_capacity(partial.fragments.iter().flatten().map(Vec::len).sum());
        for f in partial.fragments.into_iter().flatten() {
            data.extend_from_slice(&f);
        }
        Some(Frame { header: partial.header, data, first_us: partial.first_us, complete_us: now_us })
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

    fn give_up_through(&mut self, i: usize) {
        let frame = self.partial[i].header.frame;
        self.partial.drain(..=i);
        self.account(frame, i as u32);
        self.lost += 1;
    }

    /// Moves the mark to `frame`, counting the frames skipped on the way.
    /// With no mark yet, only the `dropped` partials are known to be lost.
    fn account(&mut self, frame: u32, dropped: u32) {
        self.lost += match self.mark {
            Some(mark) => frame.wrapping_sub(mark).wrapping_sub(1),
            None => dropped,
        };
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
        let dgrams = Packetizer::new().packetize(&frame(&data, true), 1200);
        assert_eq!(dgrams.len(), 3);
        assert!(dgrams.iter().all(|d| d.len() <= 1200));
        let mut rx = Reassembler::new(50_000);
        let mut out = None;
        // Out of order is fine.
        for d in dgrams.iter().rev() {
            let (h, p) = split(d);
            out = rx.push(h, &p, 5);
        }
        let out = out.unwrap();
        assert_eq!(out.data, data);
        assert!(out.header.keyframe());
        assert_eq!(rx.take_lost(), 0);
    }

    #[test]
    fn empty_frame_is_one_fragment() {
        let dgrams = Packetizer::new().packetize(&frame(&[], false), 1200);
        assert_eq!(dgrams.len(), 1);
        let (h, p) = split(&dgrams[0]);
        assert_eq!(Reassembler::new(1).push(h, &p, 0).unwrap().data, b"");
    }

    #[test]
    fn incomplete_frame_is_superseded_and_counted() {
        let mut tx = Packetizer::new();
        let mut rx = Reassembler::new(50_000);
        let f0 = tx.packetize(&frame(&[0; 3000], true), 1200);
        let f1 = tx.packetize(&frame(&[1; 100], false), 1200);
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
        let all: Vec<_> = (0..4).map(|_| tx.packetize(&frame(&[7; 10], false), 1200)).collect();
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
        let f = tx.packetize(&frame(&[0; 3000], true), 1200);
        let (h, p) = split(&f[0]);
        rx.push(h, &p, 0);
        rx.expire(10_000);
        assert_eq!(rx.take_lost(), 0);
        rx.expire(60_000);
        assert_eq!(rx.take_lost(), 1);
        // Its late fragments are stale, and the next frame isn't a gap.
        let (h, p) = split(&f[1]);
        assert!(rx.push(h, &p, 60_001).is_none());
        let (h, p) = split(&tx.packetize(&frame(&[1], false), 1200)[0]);
        assert!(rx.push(h, &p, 60_002).is_some());
        assert_eq!(rx.take_lost(), 0);
    }

    #[test]
    fn frame_numbers_wrap() {
        let mut tx = Packetizer { next_frame: u32::MAX };
        let mut rx = Reassembler::new(50_000);
        for expect in [u32::MAX, 0, 1] {
            let (h, p) = split(&tx.packetize(&frame(&[1], false), 1200)[0]);
            assert_eq!(rx.push(h, &p, 0).unwrap().header.frame, expect);
        }
        assert_eq!(rx.take_lost(), 0);
    }
}
