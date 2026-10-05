//! Latency of each presented frame, broken into stages and logged every few
//! seconds. Times are on the client's clock; the server's capture time is
//! converted with the offset measured by ping (`farsight_client`).
//!
//! "total" runs from the nested compositor's commit on the server to the
//! client's buffer swap. The client compositor's own wait for its next
//! refresh and the display's scanout come on top.

use farsight_client::Stats;

const REPORT_EVERY_US: u64 = 5_000_000;

pub struct Sample {
    /// Commit to encoded, on the server.
    pub encode_us: u64,
    /// The commit, on the client's clock.
    pub capture_local_us: u64,
    pub complete_us: u64,
    pub decoded_us: u64,
    pub presented_us: u64,
}

#[derive(Default)]
pub struct Latency {
    stages: [Vec<u64>; 5],
    since_us: u64,
    last: Stats,
}

const NAMES: [&str; 5] = ["encode", "network", "decode", "present", "total"];

impl Latency {
    pub fn add(&mut self, s: Sample) {
        let encoded = s.capture_local_us + s.encode_us;
        let values = [
            s.encode_us,
            s.complete_us.saturating_sub(encoded),
            s.decoded_us.saturating_sub(s.complete_us),
            s.presented_us.saturating_sub(s.decoded_us),
            s.presented_us.saturating_sub(s.capture_local_us),
        ];
        for (v, stage) in values.into_iter().zip(&mut self.stages) {
            stage.push(v);
        }
    }

    /// Logs a summary every few seconds; `stats` is only called then.
    pub fn maybe_report(&mut self, now_us: u64, stats: impl FnOnce() -> Stats) {
        if self.since_us == 0 {
            self.since_us = now_us;
        }
        let elapsed = now_us.saturating_sub(self.since_us);
        if elapsed < REPORT_EVERY_US {
            return;
        }
        let stats = &stats();
        let frames = self.stages[0].len();
        let summary: Vec<String> = NAMES
            .iter()
            .zip(&mut self.stages)
            .filter(|(_, v)| !v.is_empty())
            .map(|(name, v)| {
                v.sort_unstable();
                let pct = |p: f64| v[((v.len() - 1) as f64 * p) as usize] as f64 / 1000.0;
                format!("{name} {:.1}/{:.1}", pct(0.5), pct(0.95))
            })
            .collect();
        tracing::info!(
            fps = format!("{:.1}", frames as f64 * 1e6 / elapsed as f64),
            rtt_ms = format!("{:.2}", stats.rtt_us as f64 / 1000.0),
            rtt_max_ms = format!("{:.2}", stats.rtt_max_us as f64 / 1000.0),
            lost = stats.lost - self.last.lost,
            recovered = stats.recovered - self.last.recovered,
            keyframe_requests = stats.keyframe_requests - self.last.keyframe_requests,
            "latency ms, median/p95: {}",
            summary.join(", ")
        );
        for v in &mut self.stages {
            v.clear();
        }
        self.since_us = now_us;
        self.last = *stats;
    }
}
