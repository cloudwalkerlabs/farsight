//! What the sender knows about a connection's path, from quinn's own
//! accounting: how many packets it sent and how many it declared lost.
//! FEC is sized from the loss (`docs/design.md` §2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use quinn::Connection;

/// How often the path is sampled.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// A sample needs this many packets sent to say anything about loss;
/// quieter intervals are pooled with the next.
const MIN_PACKETS: u64 = 50;

/// Smoothing per sample: loss is believed sooner when it rises than it is
/// forgotten, since parity that comes too late is no use. Faster still,
/// and the estimate follows the noise of single samples.
const RISE: f64 = 0.25;
const FALL: f64 = 0.08;

/// Turns cumulative sent and lost counts into a smoothed loss rate.
#[derive(Debug, Default)]
pub struct LossMeter {
    sent: u64,
    lost: u64,
    loss: f64,
}

impl LossMeter {
    /// Takes the counts so far, and returns the loss rate (0–1).
    pub fn update(&mut self, sent: u64, lost: u64) -> f64 {
        let (ds, dl) = (sent.saturating_sub(self.sent), lost.saturating_sub(self.lost));
        if ds < MIN_PACKETS {
            return self.loss;
        }
        (self.sent, self.lost) = (sent, lost);
        let sample = (dl as f64 / ds as f64).min(1.0);
        let weight = if sample > self.loss { RISE } else { FALL };
        self.loss += (sample - self.loss) * weight;
        self.loss
    }

    pub fn loss(&self) -> f64 {
        self.loss
    }
}

/// A connection's path as last measured, shared with whoever sends on it.
#[derive(Debug, Default)]
pub struct PathState {
    /// Parts per million.
    loss_ppm: AtomicU32,
}

impl PathState {
    /// The packet loss rate, 0–1.
    pub fn loss(&self) -> f64 {
        self.loss_ppm.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// The path is logged this often, in samples.
const LOG_EVERY: u32 = 50;

/// Samples `conn` every [`INTERVAL`] until it closes.
pub fn monitor(conn: Connection) -> Arc<PathState> {
    let state = Arc::new(PathState::default());
    let shared = state.clone();
    tokio::spawn(async move {
        let mut meter = LossMeter::default();
        let mut tick = tokio::time::interval(INTERVAL);
        let mut last = conn.stats().path;
        for n in 1.. {
            tokio::select! {
                _ = tick.tick() => {}
                _ = conn.closed() => return,
            }
            let p = conn.stats().path;
            let loss = meter.update(p.sent_packets - p.sent_plpmtud_probes, p.lost_packets);
            shared.loss_ppm.store((loss * 1e6) as u32, Ordering::Relaxed);
            if n % LOG_EVERY == 0 {
                let (sent, lost) = (p.sent_packets - last.sent_packets, p.lost_packets - last.lost_packets);
                tracing::info!(
                    loss = format!("{:.2}%", loss * 100.0),
                    sent,
                    lost,
                    rtt_ms = format!("{:.2}", p.rtt.as_secs_f64() * 1e3),
                    "path"
                );
                last = p;
            }
        }
    });
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rises_fast_and_falls_slowly() {
        let mut m = LossMeter::default();
        let (mut sent, mut lost) = (0, 0);
        let mut step = |m: &mut LossMeter, s: u64, l: u64| {
            sent += s;
            lost += l;
            m.update(sent, lost)
        };
        assert_eq!(step(&mut m, 100, 0), 0.0);
        let up = step(&mut m, 100, 20);
        assert!((up - 0.05).abs() < 1e-9, "{up}");
        let down = step(&mut m, 100, 0);
        assert!(down > 0.045, "{down}");
        // Too few packets to tell: pooled with the next sample.
        assert_eq!(step(&mut m, 25, 25), down);
        assert!(step(&mut m, 25, 0) > down);
    }
}
