//! Audio playback (`docs/design.md` §8): a jitter buffer in front of the
//! Opus decoder, with drift correction by resampling.
//!
//! Unlike video, audio can't pause between packets, so it is buffered: just
//! enough to cover the jitter seen recently, plus what the output takes
//! per callback. Each datagram repeats the two frames before its own, so a
//! lost datagram usually costs nothing; a frame that never arrives is
//! concealed by Opus. The server's clock and the sound card's drift apart
//! by tens of ppm, so the buffer is kept at its target by playing slightly
//! faster or slower (at most 0.5%, after a burst), never by dropping
//! samples.
//!
//! [`Player`] has no clock or device of its own: the network side
//! [`push`](Player::push)es datagrams as they arrive and the output callback
//! [`pull`](Player::pull)s samples, each with the time.

use std::collections::VecDeque;

use farsight_proto::audio::{AudioConfig, AudioPacket};

/// Frames tracked ahead of the one playing next: one second at 5 ms.
const WINDOW: usize = 200;

/// Loss concealed in a row before the player stops and buffers again.
const MAX_CONCEALED: u32 = 4;

/// Arrivals kept for the jitter estimate: two seconds at 5 ms.
const JITTER_SAMPLES: usize = 400;

/// Within this of its target, the buffer is left alone (in µs).
const DEADBAND_US: f64 = 1500.0;

/// Speed change per µs the buffer is off target, and its limit: 10 ms too
/// much drains in about three seconds.
const GAIN: f64 = 0.3e-6;
const MAX_ADJUST: f64 = 0.005;

/// More than this past the target, and whole frames are dropped to catch
/// up: the network or the output stalled, and everything arrived at once.
const OVERFLOW_US: u64 = 40_000;

#[derive(Debug)]
struct Slot {
    /// Server µs of the frame's first sample.
    capture_us: u64,
    /// Opus; empty for digital silence.
    data: Vec<u8>,
}

/// Running statistics, reset by [`Player::take_stats`].
#[derive(Debug, Default, Clone)]
pub struct AudioStats {
    /// Server capture to the speaker, per pull, in µs; needs the clock
    /// offset.
    pub latency_us: Vec<u64>,
    /// The buffer's level and target when last pulled, in µs.
    pub buffered_us: u64,
    pub target_us: u64,
    pub concealed: u64,
    /// Frames the server sent as digital silence.
    pub silent: u64,
    pub underruns: u64,
    /// Frames dropped to catch up after a stall.
    pub skipped: u64,
    /// The playback speed, minus one, last applied.
    pub adjust: f64,
    /// The output's own latency, last reported, in µs.
    pub output_delay_us: u64,
}

pub struct Player {
    config: AudioConfig,
    channels: usize,
    frame_samples: usize,
    decoder: opus::Decoder,
    /// Frames from `next_seq` on.
    slots: VecDeque<Option<Slot>>,
    next_seq: Option<u32>,
    /// Playing, as opposed to buffering.
    started: bool,
    /// Decoded samples, interleaved, and the capture time of the first.
    fifo: VecDeque<f32>,
    fifo_capture_us: f64,
    /// Where between the fifo's first two sample frames playback is.
    frac: f64,
    last_silent: bool,
    concealed_run: u32,
    /// Arrival minus capture, for the newest frame of recent datagrams.
    transit: VecDeque<i64>,
    jitter_us: u64,
    /// Samples per channel the output asked for last.
    request: usize,
    stats: AudioStats,
    scratch: Vec<f32>,
}

impl Player {
    pub fn new(config: AudioConfig) -> anyhow::Result<Self> {
        let channels = match config.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            n => anyhow::bail!("{n} audio channels"),
        };
        let frame_samples = config.frame_samples();
        Ok(Self {
            config,
            channels: config.channels as usize,
            frame_samples,
            decoder: opus::Decoder::new(config.sample_rate, channels)?,
            slots: VecDeque::new(),
            next_seq: None,
            started: false,
            fifo: VecDeque::new(),
            fifo_capture_us: 0.0,
            frac: 0.0,
            last_silent: false,
            concealed_run: 0,
            transit: VecDeque::new(),
            jitter_us: 0,
            request: frame_samples,
            stats: AudioStats::default(),
            scratch: vec![0.0; frame_samples * config.channels as usize],
        })
    }

    pub fn config(&self) -> AudioConfig {
        self.config
    }

    /// A datagram arrived at `arrival_us`, on any clock that runs at the
    /// server's rate (offsets cancel out).
    pub fn push(&mut self, p: &AudioPacket, arrival_us: u64) {
        if p.discontinuity() && self.next_seq.is_some_and(|n| p.seq.wrapping_sub(n) as i32 >= 0) {
            self.restart();
        }
        if self.next_seq.is_none() {
            self.next_seq = Some(p.seq);
        }
        let next = self.next_seq.unwrap();
        for (i, data) in p.frames.iter().enumerate() {
            let seq = p.seq.wrapping_sub(i as u32);
            let ahead = seq.wrapping_sub(next) as i32;
            if ahead < 0 {
                continue; // played, or concealed, already
            }
            let ahead = ahead as usize;
            if ahead >= WINDOW {
                // Far ahead: a long stall, or a stream we lost track of.
                self.restart();
                return self.push(p, arrival_us);
            }
            if self.slots.len() <= ahead {
                self.slots.resize_with(ahead + 1, || None);
            }
            let slot = &mut self.slots[ahead];
            if slot.is_none() {
                let capture_us = p.capture_us.saturating_sub(i as u64 * self.config.frame_us as u64);
                *slot = Some(Slot { capture_us, data: data.to_vec() });
            }
        }
        self.note_arrival(arrival_us as i64 - p.capture_us as i64);
    }

    /// The 95th percentile of transit time above its minimum, over the
    /// last couple of seconds.
    fn note_arrival(&mut self, transit: i64) {
        if self.transit.len() == JITTER_SAMPLES {
            self.transit.pop_front();
        }
        self.transit.push_back(transit);
        let base = *self.transit.iter().min().unwrap();
        let mut d: Vec<u64> = self.transit.iter().map(|t| (t - base) as u64).collect();
        let k = (d.len() * 95 / 100).min(d.len() - 1);
        self.jitter_us = *d.select_nth_unstable(k).1;
    }

    fn restart(&mut self) {
        self.slots.clear();
        self.next_seq = None;
        self.started = false;
        self.concealed_run = 0;
        let _ = self.decoder.reset_state();
    }

    fn us(&self, samples: usize) -> u64 {
        samples as u64 * 1_000_000 / self.config.sample_rate as u64
    }

    /// Samples per channel buffered: decoded, plus every frame up to the
    /// newest that arrived.
    fn level(&self) -> usize {
        self.fifo.len() / self.channels + self.slots.len() * self.frame_samples
    }

    /// What the buffer aims for, in samples per channel: one output
    /// request, the jitter, and half a frame of margin.
    fn target(&self) -> usize {
        let jitter = (self.jitter_us * self.config.sample_rate as u64 / 1_000_000) as usize;
        self.request + jitter.max(self.frame_samples) + self.frame_samples / 2
    }

    /// Decodes the next frame into the fifo. False if there is nothing to
    /// play: buffering again.
    fn decode_next(&mut self) -> bool {
        let Some(seq) = self.next_seq else { return false };
        let slot = self.slots.pop_front().flatten();
        let more = !self.slots.is_empty();
        let capture_us = match slot {
            Some(Slot { capture_us, data }) => {
                self.concealed_run = 0;
                self.last_silent = data.is_empty();
                if data.is_empty() {
                    self.stats.silent += 1;
                    self.scratch.fill(0.0);
                } else if let Err(err) = self.decoder.decode_float(&data, &mut self.scratch, false) {
                    tracing::debug!(%err, "opus");
                    self.scratch.fill(0.0);
                }
                capture_us
            }
            None if self.last_silent && !more => {
                // The server stopped after silence: wait, quietly.
                self.started = false;
                return false;
            }
            None => {
                let capture_us = self.fifo_capture_us as u64 + self.us(self.fifo.len() / self.channels);
                if !more && self.concealed_run >= MAX_CONCEALED {
                    self.stats.underruns += 1;
                    self.started = false;
                    self.concealed_run = 0;
                    return false;
                }
                self.concealed_run += 1;
                self.stats.concealed += 1;
                if self.decoder.decode_float(&[], &mut self.scratch, false).is_err() {
                    self.scratch.fill(0.0);
                }
                capture_us
            }
        };
        // The server's timestamps are the truth: re-anchor the fifo's
        // first sample to the frame just decoded.
        let ahead = self.fifo.len() / self.channels;
        self.fifo_capture_us = capture_us as f64 - ahead as f64 * 1e6 / self.config.sample_rate as f64;
        self.fifo.extend(self.scratch.iter().copied());
        self.next_seq = Some(seq.wrapping_add(1));
        true
    }

    /// Fills `out` (interleaved) for playback at `play_local_us` on the
    /// client's clock. `offset_us` (server minus client) gives latency
    /// statistics once known.
    pub fn pull(&mut self, out: &mut [f32], now_local_us: u64, output_delay_us: u64, offset_us: Option<i64>) {
        self.stats.output_delay_us = output_delay_us;
        let play_local_us = now_local_us + output_delay_us;
        let ch = self.channels;
        let want = out.len() / ch;
        self.request = want.max(1);
        let target = self.target();
        let level = self.level();
        self.stats.buffered_us = self.us(level);
        self.stats.target_us = self.us(target);
        if !self.started {
            if level >= target && self.next_seq.is_some() {
                // Start at the target, not with whatever piled up while
                // the output was opening.
                let mut level = level;
                while level >= target + self.frame_samples && self.slots.len() > 1 {
                    self.slots.pop_front();
                    self.next_seq = self.next_seq.map(|s| s.wrapping_add(1));
                    level -= self.frame_samples;
                }
                self.started = true;
                return self.pull(out, now_local_us, output_delay_us, offset_us);
            } else {
                out.fill(0.0);
                return;
            }
        }
        // Too far behind: drop whole frames, rather than speeding up for
        // seconds.
        let mut level = level;
        while self.us(level) > self.us(target) + OVERFLOW_US && self.slots.len() > 1 {
            self.slots.pop_front();
            self.next_seq = self.next_seq.map(|s| s.wrapping_add(1));
            self.stats.skipped += 1;
            level -= self.frame_samples;
        }
        let error_us = self.us(level) as f64 - self.us(target) as f64;
        let adjust = if error_us.abs() < DEADBAND_US { 0.0 } else { (error_us * GAIN).clamp(-MAX_ADJUST, MAX_ADJUST) };
        self.stats.adjust = adjust;
        let step = 1.0 + adjust;

        if let Some(offset) = offset_us {
            let server_now = (play_local_us as i64 + offset) as u64;
            self.stats.latency_us.push(server_now.saturating_sub(self.fifo_capture_us as u64));
        }
        for frame in out.chunks_exact_mut(ch) {
            // Four sample frames around the playback position.
            while self.fifo.len() < 4 * ch {
                if !self.decode_next() {
                    break;
                }
            }
            if self.fifo.len() < 4 * ch {
                frame.fill(0.0);
                continue;
            }
            let t = self.frac as f32;
            for (c, o) in frame.iter_mut().enumerate() {
                let p = |i: usize| self.fifo[i * ch + c];
                *o = hermite(p(0), p(1), p(2), p(3), t);
            }
            self.frac += step;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                for _ in 0..ch {
                    self.fifo.pop_front();
                }
                self.fifo_capture_us += 1e6 / self.config.sample_rate as f64;
            }
        }
    }

    pub fn take_stats(&mut self) -> AudioStats {
        let stats = self.stats.clone();
        self.stats.latency_us.clear();
        self.stats.concealed = 0;
        self.stats.silent = 0;
        self.stats.underruns = 0;
        self.stats.skipped = 0;
        stats
    }
}

/// Cubic Hermite (Catmull-Rom) between `b` and `c`, at `t` in [0, 1).
fn hermite(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let c1 = 0.5 * (c - a);
    let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
    let c3 = 0.5 * (d - a) + 1.5 * (b - c);
    ((c3 * t + c2) * t + c1) * t + b
}

#[cfg(test)]
mod tests {
    use super::*;
    use farsight_proto::audio::{FLAG_DISCONTINUITY, SAMPLE_RATE};

    const CONFIG: AudioConfig = AudioConfig { channels: 2, sample_rate: SAMPLE_RATE, frame_us: 5000 };

    struct Server {
        encoder: opus::Encoder,
        recent: Vec<Vec<u8>>,
        seq: u32,
    }

    impl Server {
        fn new() -> Self {
            let encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
            Self { encoder, recent: Vec::new(), seq: 0 }
        }

        /// Frame `seq` of a 440 Hz tone, as a datagram's bytes.
        fn next(&mut self) -> Vec<u8> {
            let pcm: Vec<f32> = (0..240)
                .flat_map(|i| {
                    let t = (self.seq as usize * 240 + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                    [v, v]
                })
                .collect();
            let mut out = vec![0; 1500];
            let n = self.encoder.encode_float(&pcm, &mut out).unwrap();
            out.truncate(n);
            self.recent.insert(0, out);
            self.recent.truncate(3);
            let p = AudioPacket {
                flags: if self.seq == 0 { FLAG_DISCONTINUITY } else { 0 },
                seq: self.seq,
                capture_us: self.seq as u64 * 5000,
                frames: self.recent.iter().map(Vec::as_slice).collect(),
            };
            self.seq += 1;
            let mut buf = Vec::new();
            p.write(&mut buf);
            buf
        }
    }

    fn push(player: &mut Player, buf: &[u8], at: u64) {
        player.push(&AudioPacket::read(buf).unwrap(), at);
    }

    #[test]
    fn plays_once_buffered_and_survives_loss() {
        let mut server = Server::new();
        let mut player = Player::new(CONFIG).unwrap();
        let mut out = vec![0.0; 480];
        // Nothing buffered: silence.
        player.pull(&mut out, 0, 0, None);
        assert!(out.iter().all(|&s| s == 0.0));
        let mut energy = 0.0;
        for i in 0..200u64 {
            let d = server.next();
            // Every fifth datagram lost: the next one's repeats cover it.
            if i % 5 != 3 {
                push(&mut player, &d, i * 5000 + 1000);
            }
            player.pull(&mut out, i * 5000 + 2000, 0, Some(0));
            if i > 20 {
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
        }
        let stats = player.take_stats();
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.underruns, 0);
        assert!(energy > 1000.0, "energy {energy}");
        // A frame or two of buffer, and a few ms of latency.
        let median = stats.latency_us[stats.latency_us.len() / 2];
        assert!((5_000..25_000).contains(&median), "latency {median}");
    }

    #[test]
    fn a_slow_clock_is_corrected_by_speed() {
        let mut server = Server::new();
        let mut player = Player::new(CONFIG).unwrap();
        let mut out = vec![0.0; 480];
        // The server runs 0.1% fast: datagrams come a little more often
        // than the output consumes them.
        let (mut skipped, mut sped_up) = (0, false);
        for i in 0..4000u64 {
            let at = i * 4995;
            push(&mut player, &server.next(), at);
            player.pull(&mut out, at, 0, None);
            let s = player.take_stats();
            skipped += s.skipped;
            sped_up |= s.adjust > 0.0;
        }
        // Uncorrected, 20 s at 0.1% would have added 20 ms.
        assert_eq!(skipped, 0);
        assert!(sped_up);
        let s = player.take_stats();
        assert!(s.buffered_us < s.target_us + 5000, "{s:?}");
    }

    #[test]
    fn hermite_passes_through_points() {
        assert_eq!(hermite(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
        assert!((hermite(0.0, 1.0, 2.0, 3.0, 0.5) - 1.5).abs() < 1e-6);
    }
}
