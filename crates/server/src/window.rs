//! Keeps the encoder within what each client decodes (`docs/design.md` §1).
//!
//! Congestion control and the pipeline's skip keep frames from queueing in
//! the network, but a client whose decoder is slower than the frame rate
//! queues them in front of it instead, and nothing there drops them: a
//! decoder needs every frame since the last keyframe. So each client says
//! what it has decoded (`Decoded` datagrams, by capture time). A frame it
//! hasn't decoded a round trip after its last byte left is at its decoder,
//! being decoded or waiting. Those, and frames being encoded, are kept to
//! [`AT_DECODER`]: the pipeline skips encoding past that, as it does for
//! the network. The decoder always has the next frame ready, however fast
//! it is, and no more queued. Frames still crossing the network don't
//! count, so a long path is kept full.
//!
//! A frame at the decoder for [`GIVE_UP`] is taken as lost, so a client
//! that stops answering holds nothing back for longer. One that has never
//! answered isn't waited for at all.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Frames that may be at a client's decoder or on their way into it from
/// the encoder: the one it decodes, and the next.
pub const AT_DECODER: usize = 2;

/// A frame at the decoder this long is lost, not waited for.
pub const GIVE_UP: Duration = Duration::from_millis(250);

/// No frame, in [`Arrivals`].
const NONE: u64 = u64::MAX;

/// When a client's oldest frames not yet decoded are at its decoder, in
/// host µs, oldest first; [`NONE`] past the last.
pub type Arrivals = [u64; AT_DECODER];

/// One client's frames sent and not yet decoded.
#[derive(Debug, Default)]
pub struct Window {
    /// Oldest first: capture time, and when it is at the decoder, in host
    /// µs.
    waiting: VecDeque<(u64, u64)>,
    /// The client says what it decodes.
    answers: bool,
}

impl Window {
    /// The frame or update captured at `capture_us` went out; it is at the
    /// client's decoder by `arrives_us`, unless decoded already.
    pub fn sent(&mut self, capture_us: u64, arrives_us: u64) {
        self.waiting.push_back((capture_us, arrives_us));
    }

    /// The client decoded everything captured up to `capture_us`.
    pub fn decoded(&mut self, capture_us: u64) {
        self.answers = true;
        while self.waiting.front().is_some_and(|&(c, _)| c <= capture_us) {
            self.waiting.pop_front();
        }
    }

    /// When the oldest frames not yet decoded are at the decoder, or `None`
    /// if the client isn't waited for. Forgets frames given up on by
    /// `now_us`.
    pub fn arrivals(&mut self, now_us: u64) -> Option<Arrivals> {
        let give_up = GIVE_UP.as_micros() as u64;
        while self.waiting.front().is_some_and(|&(_, at)| at + give_up <= now_us) {
            self.waiting.pop_front();
        }
        if !self.answers {
            return None;
        }
        let mut out = [NONE; AT_DECODER];
        for (o, &(_, at)) in out.iter_mut().zip(&self.waiting) {
            *o = at;
        }
        Some(out)
    }
}

/// Frames at the decoder by `now_us`, of `arrivals`, and not given up on.
fn at_decoder(arrivals: &Arrivals, now_us: u64) -> usize {
    let give_up = GIVE_UP.as_micros() as u64;
    arrivals.iter().filter(|&&at| at <= now_us && now_us < at.saturating_add(give_up)).count()
}

/// What the network tells the pipeline about the clients' decoders.
#[derive(Debug)]
pub struct Decoding {
    /// The earliest [`Arrivals`] of every client that answers.
    arrivals: [AtomicU64; AT_DECODER],
    /// The pipeline skipped a frame for a client's decoder, and wants to
    /// hear when there may be room.
    waiting: AtomicBool,
}

impl Default for Decoding {
    fn default() -> Self {
        Self { arrivals: std::array::from_fn(|_| AtomicU64::new(NONE)), waiting: AtomicBool::new(false) }
    }
}

impl Decoding {
    /// Sets the clients' arrivals, the earliest of each, at `now_us`.
    /// Returns whether the pipeline was waiting and should hear that the
    /// decoders have room.
    pub fn set(&self, clients: impl IntoIterator<Item = Arrivals>, now_us: u64) -> bool {
        let mut earliest = [NONE; AT_DECODER];
        for a in clients {
            for (e, at) in earliest.iter_mut().zip(a) {
                *e = (*e).min(at);
            }
        }
        for (slot, at) in self.arrivals.iter().zip(earliest) {
            slot.store(at, Ordering::SeqCst);
        }
        at_decoder(&earliest, now_us) < AT_DECODER && self.waiting.swap(false, Ordering::SeqCst)
    }

    /// Whether a new frame would overfill a client's decoder at `now_us`,
    /// with `encoding` frames on their way from the encoder; if so, until
    /// when at most. The pipeline hears when there is room, once, after
    /// asking.
    pub fn behind(&self, now_us: u64, encoding: usize) -> Option<u64> {
        // Asked for first, so room made in between is heard of.
        self.waiting.store(true, Ordering::SeqCst);
        let arrivals: Arrivals = std::array::from_fn(|i| self.arrivals[i].load(Ordering::SeqCst));
        if at_decoder(&arrivals, now_us) > 0 && at_decoder(&arrivals, now_us) + encoding >= AT_DECODER {
            return Some(arrivals[0].saturating_add(GIVE_UP.as_micros() as u64));
        }
        self.waiting.store(false, Ordering::SeqCst);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    fn window(sent: &[(u64, u64)]) -> Window {
        let mut w = Window::default();
        w.decoded(0);
        for &(c, at) in sent {
            w.sent(c, at);
        }
        w
    }

    #[test]
    fn a_client_that_never_answers_is_not_waited_for() {
        let mut w = Window::default();
        for c in 1..=5 {
            w.sent(c, c * MS);
        }
        assert_eq!(w.arrivals(10 * MS), None);
        let d = Decoding::default();
        d.set(w.arrivals(10 * MS), 10 * MS);
        assert_eq!(d.behind(10 * MS, 2), None);
    }

    #[test]
    fn decoding_a_frame_answers_for_those_before_it() {
        let mut w = window(&[(1, 10 * MS), (2, 20 * MS), (3, 30 * MS)]);
        assert_eq!(w.arrivals(0), Some([10 * MS, 20 * MS]));
        w.decoded(2);
        assert_eq!(w.arrivals(0), Some([30 * MS, NONE]));
        w.decoded(3);
        assert_eq!(w.arrivals(0), Some([NONE, NONE]), "still answering");
    }

    #[test]
    fn frames_at_the_decoder_too_long_are_given_up_on() {
        let mut w = window(&[(1, 10 * MS), (2, 300 * MS)]);
        let give_up = GIVE_UP.as_micros() as u64;
        assert_eq!(w.arrivals(10 * MS + give_up - 1), Some([10 * MS, 300 * MS]));
        assert_eq!(w.arrivals(10 * MS + give_up), Some([300 * MS, NONE]));
    }

    #[test]
    fn behind_with_two_frames_at_the_decoder_or_coming_from_the_encoder() {
        let d = Decoding::default();
        d.set([[10 * MS, 20 * MS]], 0);
        // Both still crossing the network: a long path is kept full.
        assert_eq!(d.behind(5 * MS, 1), None);
        // One at the decoder, and one more from the encoder would fill it.
        assert_eq!(d.behind(10 * MS, 0), None);
        let until = 10 * MS + GIVE_UP.as_micros() as u64;
        assert_eq!(d.behind(10 * MS, 1), Some(until));
        assert_eq!(d.behind(20 * MS, 0), Some(until));
        assert_eq!(d.behind(until + 20 * MS, 0), None, "given up on");
    }

    #[test]
    fn the_slowest_client_counts() {
        let d = Decoding::default();
        d.set([[10 * MS, NONE], [5 * MS, 8 * MS]], 0);
        assert!(d.behind(8 * MS, 0).is_some());
    }

    #[test]
    fn the_pipeline_hears_of_room_once_and_only_if_it_asked() {
        let d = Decoding::default();
        assert!(!d.set([[NONE, NONE]], 0), "not asked");
        d.set([[10 * MS, 15 * MS]], 0);
        assert!(d.behind(20 * MS, 0).is_some());
        assert!(!d.set([[10 * MS, 15 * MS]], 20 * MS), "still full");
        assert!(d.set([[15 * MS, NONE]], 20 * MS));
        assert!(!d.set([[NONE, NONE]], 20 * MS), "once");
    }
}
