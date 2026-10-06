//! Keeps video near the bitrate congestion control allows (`docs/design.md`
//! §1). The encoders run at a constant QP, which keeps text sharp and costs
//! whatever the picture costs. When that is more than the path takes, a QP
//! offset goes on top of it, frame by frame, and comes off again once there
//! is room. Encoders take a new QP on any frame with no keyframe: NVENC
//! through a reconfigure, VA-API through a region of interest over the
//! whole picture.
//!
//! Six more QP roughly halves the bits, so a stream at twice its target
//! gets six more at once. The offset stops at [`MAX_OFFSET`]: past that
//! the picture turns to blocks that later frames take seconds to clean
//! up, and a desktop is better at a lower frame rate. Beyond it the
//! pipeline skips frames while the network's queue drains. Once spending
//! drops, the offset falls as far as the spending allows at once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The spending rate is measured over this long.
const WINDOW: Duration = Duration::from_millis(300);

/// Unspent budget is kept up to this long at the target, so a keyframe or
/// a burst of motion after a quiet spell goes at the base QP.
const SURPLUS: Duration = Duration::from_millis(250);

/// Debt is forgotten past this long at the target.
const MAX_DEBT: Duration = Duration::from_secs(1);

/// The offset rises at most this often, so the spending measured has
/// mostly caught up with the last rise, and falls at most this often.
const RISE_EVERY: Duration = Duration::from_millis(200);
const FALL_EVERY: Duration = Duration::from_millis(250);

/// The most the offset rises above the base QP, on H.264's scale: 24
/// becomes 36, still readable.
pub const MAX_OFFSET: u32 = 12;

/// One QP less costs about this much more.
const STEP_COST: f64 = 1.12;

/// The offset falls by as many steps as would still spend under this
/// share of the target.
const FALL_BELOW: f64 = 0.95;

#[derive(Debug)]
pub struct QpControl {
    /// Bits that may still be spent; negative is debt.
    budget: f64,
    last: Option<Instant>,
    /// Frames spent in the last [`WINDOW`]: when, and bits.
    spent: VecDeque<(Instant, f64)>,
    offset: u32,
    max_offset: u32,
    changed: Option<Instant>,
}

impl QpControl {
    /// `max_offset`: how far above the base QP the codec's range goes;
    /// the offset stays within [`MAX_OFFSET`] too.
    pub fn new(max_offset: u32) -> Self {
        let max_offset = max_offset.min(MAX_OFFSET);
        Self { budget: 0.0, last: None, spent: VecDeque::new(), offset: 0, max_offset, changed: None }
    }

    /// The QP offset for a frame encoded now, with `target_bps` allowed.
    pub fn next(&mut self, now: Instant, target_bps: u64) -> u32 {
        let target = target_bps.max(1) as f64;
        let dt = self.last.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.last = Some(now);
        self.budget = (self.budget + target * dt.as_secs_f64())
            .clamp(-target * MAX_DEBT.as_secs_f64(), target * SURPLUS.as_secs_f64());
        while self.spent.front().is_some_and(|&(t, _)| now.saturating_duration_since(t) > WINDOW) {
            self.spent.pop_front();
        }
        let spending = self.spent.iter().map(|&(_, bits)| bits).sum::<f64>() / WINDOW.as_secs_f64();
        let since = self.changed.map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        if self.budget < 0.0 && spending > target && since >= RISE_EVERY {
            let steps = (6.0 * (spending / target).log2()).round().clamp(1.0, 6.0) as u32;
            self.offset = (self.offset + steps).min(self.max_offset);
            self.changed = Some(now);
        } else if self.offset > 0 && spending * STEP_COST < target * FALL_BELOW && since >= FALL_EVERY {
            // Debt doesn't hold it up: the network's queue, which the
            // pipeline skips frames for, is what an overspend costs.
            let room = if spending > 0.0 { (target * FALL_BELOW / spending).ln() / STEP_COST.ln() } else { f64::MAX };
            self.offset -= (room as u32).clamp(1, self.offset);
            self.changed = Some(now);
        }
        self.offset
    }

    /// The frame just encoded came to `bytes`.
    pub fn spent(&mut self, now: Instant, bytes: usize) {
        let bits = bytes as f64 * 8.0;
        self.budget -= bits;
        self.spent.push_back((now, bits));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An encoder whose frames halve every 6 QP.
    fn frame_bytes(at_base: f64, offset: u32) -> usize {
        (at_base * 0.5f64.powf(offset as f64 / 6.0)) as usize
    }

    fn run(qp: &mut QpControl, t: &mut Instant, frames: usize, at_base: f64, target: u64) -> f64 {
        let mut bytes = 0;
        for _ in 0..frames {
            *t += Duration::from_micros(16_667);
            let offset = qp.next(*t, target);
            let b = frame_bytes(at_base, offset);
            qp.spent(*t, b);
            bytes += b;
        }
        bytes as f64 * 8.0 / (frames as f64 / 60.0)
    }

    #[test]
    fn settles_near_the_target_and_comes_back_down() {
        let mut qp = QpControl::new(27);
        let mut t = Instant::now();
        // 60 fps of 40 KB is 19.2 Mbit/s, against 5.
        run(&mut qp, &mut t, 120, 40_000.0, 5_000_000);
        let rate = run(&mut qp, &mut t, 300, 40_000.0, 5_000_000);
        assert!((4.0e6..=5.5e6).contains(&rate), "{rate}");
        assert!((10..=14).contains(&qp.offset), "{}", qp.offset);
        // Room again: the offset comes off.
        run(&mut qp, &mut t, 600, 40_000.0, 50_000_000);
        assert_eq!(qp.offset, 0);
    }

    #[test]
    fn a_keyframe_after_quiet_goes_at_base() {
        let mut qp = QpControl::new(27);
        let mut t = Instant::now();
        run(&mut qp, &mut t, 60, 1000.0, 10_000_000);
        // 300 KB in one go is 240 ms at 10 Mbit/s, which the quiet saved.
        t += Duration::from_millis(16);
        assert_eq!(qp.next(t, 10_000_000), 0);
        qp.spent(t, 300_000);
        t += Duration::from_millis(16);
        assert_eq!(qp.next(t, 10_000_000), 0);
    }

    #[test]
    fn stops_at_the_cap() {
        let mut qp = QpControl::new(27);
        let mut t = Instant::now();
        // 64 times the target would take 36.
        run(&mut qp, &mut t, 300, 40_000.0, 300_000);
        assert_eq!(qp.offset, MAX_OFFSET);
    }

    #[test]
    fn falls_at_once_when_the_burst_ends() {
        let mut qp = QpControl::new(27);
        let mut t = Instant::now();
        run(&mut qp, &mut t, 120, 200_000.0, 20_000_000);
        assert_eq!(qp.offset, MAX_OFFSET);
        // A cursor blinking: small frames, deep in debt still. Down within
        // the window and one fall, not a step every 250 ms.
        run(&mut qp, &mut t, 36, 2_000.0, 20_000_000);
        assert_eq!(qp.offset, 0);
    }

    #[test]
    fn never_past_the_codecs_range() {
        let mut qp = QpControl::new(5);
        let mut t = Instant::now();
        run(&mut qp, &mut t, 120, 1e6, 1_000_000);
        assert_eq!(qp.offset, 5);
    }
}
