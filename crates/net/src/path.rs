//! What the sender knows about a connection's path, from quinn's own
//! accounting: how many packets it sent and how many it declared lost, and
//! the rate congestion control allows. FEC is sized from the loss
//! (`docs/design.md` §2); video is paced, and encoded, at the rate (§1).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use quinn::Connection;

use crate::cc::{RateControl, Status};

/// How often the path is sampled.
pub const INTERVAL: Duration = Duration::from_millis(50);

/// A sample needs this many packets sent to say anything about loss;
/// quieter intervals are pooled with the next.
const MIN_PACKETS: u64 = 50;

/// Smoothing per sample: loss is believed sooner when it rises than it is
/// forgotten, since parity that comes too late is no use. Faster still,
/// and the estimate follows the noise of single samples.
const RISE: f64 = 0.25;
const FALL: f64 = 0.08;

/// The plain average follows samples with this weight.
const MEAN: f64 = 0.1;

/// Loss is assumed to be this until measured, so the first frames have
/// parity; on a clean link it fades within seconds.
const INITIAL_LOSS: f64 = 0.02;

/// Turns cumulative sent and lost counts into smoothed loss rates.
#[derive(Debug)]
pub struct LossMeter {
    sent: u64,
    lost: u64,
    loss: f64,
    mean: f64,
}

impl Default for LossMeter {
    fn default() -> Self {
        Self { sent: 0, lost: 0, loss: INITIAL_LOSS, mean: 0.0 }
    }
}

impl LossMeter {
    /// Takes the counts so far, and returns the loss rate (0–1) to size
    /// FEC for: quick to rise and slow to fall.
    pub fn update(&mut self, sent: u64, lost: u64) -> f64 {
        let (ds, dl) = (sent.saturating_sub(self.sent), lost.saturating_sub(self.lost));
        if ds < MIN_PACKETS {
            return self.loss;
        }
        (self.sent, self.lost) = (sent, lost);
        let sample = (dl as f64 / ds as f64).min(1.0);
        let weight = if sample > self.loss { RISE } else { FALL };
        self.loss += (sample - self.loss) * weight;
        self.mean += (sample - self.mean) * MEAN;
        self.loss
    }

    /// The loss rate as a plain moving average: neither side favoured,
    /// for judging congestion.
    pub fn mean(&self) -> f64 {
        self.mean
    }
}

/// A connection's path as last measured, shared with whoever sends on it.
#[derive(Debug)]
pub struct PathState {
    /// Parts per million.
    loss_ppm: AtomicU32,
    rate_bps: AtomicU64,
}

impl PathState {
    /// The packet loss rate, 0–1.
    pub fn loss(&self) -> f64 {
        self.loss_ppm.load(Ordering::Relaxed) as f64 / 1e6
    }

    /// What the path takes, as congestion control estimates it.
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }
}

impl Default for PathState {
    fn default() -> Self {
        Self { loss_ppm: AtomicU32::new((INITIAL_LOSS * 1e6) as u32), rate_bps: AtomicU64::default() }
    }
}

/// The path is logged this often, in samples.
const LOG_EVERY: u32 = 100;

/// Samples `conn` every [`INTERVAL`] until it closes: its loss, and the
/// rate its congestion controller ([`crate::cc`]) may send at, up to
/// `max_rate_bps`. `on_update` hears each new measurement.
pub fn monitor(
    conn: Connection,
    max_rate_bps: u64,
    on_update: impl Fn(&PathState) + Send + 'static,
) -> Arc<PathState> {
    let state = Arc::new(PathState::default());
    let shared = state.clone();
    let mut rc = RateControl::new(max_rate_bps);
    shared.rate_bps.store(rc.rate_bps(), Ordering::Relaxed);
    tokio::spawn(async move {
        let cc = crate::cc::shared(&conn);
        let mut meter = LossMeter::default();
        let mut tick = tokio::time::interval(INTERVAL);
        let mut last = conn.stats().path;
        let mut last_at = Instant::now();
        let mut status = Status::default();
        for n in 1.. {
            tokio::select! {
                _ = tick.tick() => {}
                _ = conn.closed() => return,
            }
            let now = Instant::now();
            let p = conn.stats().path;
            let loss = meter.update(p.sent_packets - p.sent_plpmtud_probes, p.lost_packets);
            shared.loss_ppm.store((loss * 1e6) as u32, Ordering::Relaxed);
            if let Some(cc) = &cc {
                let rate;
                (rate, status) = rc.update(now, now - last_at, cc.take(), meter.mean());
                cc.set_rate_bps(rate);
                shared.rate_bps.store(rate, Ordering::Relaxed);
            }
            last_at = now;
            on_update(&shared);
            if n % LOG_EVERY == 0 {
                let (sent, lost) = (p.sent_packets - last.sent_packets, p.lost_packets - last.lost_packets);
                let ms = |d: Option<Duration>| d.map_or("-".into(), |d| format!("{:.1}", d.as_secs_f64() * 1e3));
                tracing::info!(
                    rate_mbps = format!("{:.1}", shared.rate_bps() as f64 / 1e6),
                    sending_mbps = format!("{:.1}", status.sending_bps / 1e6),
                    loss = format!("{:.2}%", loss * 100.0),
                    sent,
                    lost,
                    queue_ms = ms(status.queue),
                    base_rtt_ms = ms(status.base),
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
        let mut m = LossMeter { loss: 0.0, ..LossMeter::default() };
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
