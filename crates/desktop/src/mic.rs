//! The microphone (§8), while an app in the session records: captured
//! through the desktop's sound server (cpal), echo cancelled by WebRTC's
//! AEC3 (sonora, in Rust), and handed to the client core 10 ms at a time.
//!
//! The echo to cancel is the session's own audio coming back from the
//! speakers, so the far end is what this client plays: the output callback
//! queues it, with when it will be heard, in a [`Reference`]. The two
//! streams are lined up by time, not by counting samples: an output that
//! underruns plays silence its callback never saw, and a capture callback
//! can be lost, and either would shift the far end against its echo for
//! good.
//!
//! AEC3 must see the far end before its echo, or its filter never
//! converges and after six seconds it decides there is no echo to cancel
//! (a headset, it thinks). So before each frame of the microphone, AEC3
//! gets the far end that will have played up to [`LEAD_US`] past it: in
//! practice all that is queued, which the output callback fills only a
//! little ahead of playback. The speaker's own latency and the room then
//! put the echo well after it; AEC3 finds that delay itself.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use farsight_client::Client;
use farsight_proto::audio::MIC;

/// How often the canceller's statistics are logged.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Samples in one queued piece of the far end.
const PIECE: usize = 1024;

/// Pieces queued: about 1.4 s, while the microphone's worker is behind.
const PIECES: usize = 64;

/// The far end is kept this long, at most, waiting for its echo.
const MAX_FAR_US: u64 = 500_000;

/// Gaps in the far end's timestamps shorter than this are jitter, not
/// silence played.
const GAP_US: u64 = 2_000;

/// How far ahead of the microphone the far end may go to AEC3, which
/// finds delays of up to about half a second.
const LEAD_US: u64 = 50_000;

/// Some of what the output played, mono.
struct Piece {
    /// When its first sample is heard, on the client's clock.
    play_us: u64,
    len: usize,
    samples: [f32; PIECE],
}

/// What this client plays, for the echo canceller: a lock-free queue from
/// the output callback to the microphone's worker.
#[derive(Default)]
pub struct Reference {
    /// While the microphone is open. Only the output callback locks it,
    /// but for the moments the microphone opens and closes.
    queue: Mutex<Option<rtrb::Producer<Piece>>>,
}

impl Reference {
    /// From the output callback: samples to be heard from `play_us` on (the
    /// client's clock), interleaved.
    pub fn push(&self, interleaved: &[f32], channels: usize, play_us: u64) {
        let Ok(mut queue) = self.queue.try_lock() else { return };
        let Some(queue) = queue.as_mut() else { return };
        for (i, frames) in interleaved.chunks(PIECE * channels).enumerate() {
            let mut piece = Piece {
                play_us: play_us + (i * PIECE) as u64 * 1_000_000 / MIC.sample_rate as u64,
                len: frames.len() / channels,
                samples: [0.0; PIECE],
            };
            for (s, f) in piece.samples.iter_mut().zip(frames.chunks_exact(channels)) {
                *s = f.iter().sum::<f32>() / channels as f32;
            }
            // Full only if the worker has stopped.
            let _ = queue.push(piece);
        }
    }

    /// Starts queueing what plays.
    fn open(&self) -> rtrb::Consumer<Piece> {
        let (producer, consumer) = rtrb::RingBuffer::new(PIECES);
        *self.queue.lock().unwrap() = Some(producer);
        consumer
    }

    fn close(&self) {
        *self.queue.lock().unwrap() = None;
    }
}

/// The far end on its timeline, as the worker sees it: what played, with
/// silence where the output played nothing of ours.
#[derive(Default)]
struct FarEnd {
    samples: VecDeque<f32>,
    /// When `samples[0]` was heard.
    start_us: u64,
}

impl FarEnd {
    fn us(n: usize) -> u64 {
        n as u64 * 1_000_000 / MIC.sample_rate as u64
    }

    fn add(&mut self, play_us: u64, samples: &[f32]) {
        let end_us = self.start_us + Self::us(self.samples.len());
        if self.samples.is_empty() {
            self.start_us = play_us;
        } else if play_us > end_us + GAP_US {
            let gap = ((play_us - end_us) * MIC.sample_rate as u64 / 1_000_000) as usize;
            let gap = gap.min(MIC.sample_rate as usize);
            self.samples.extend(std::iter::repeat_n(0.0, gap));
        }
        self.samples.extend(samples);
    }

    /// The next frame, if it was heard by `until_us`. Frames long past are
    /// dropped: AEC3 couldn't line them up any more.
    fn frame(&mut self, until_us: u64, out: &mut [f32]) -> bool {
        let frame_us = Self::us(out.len());
        while self.samples.len() >= out.len() && self.start_us + MAX_FAR_US < until_us {
            self.samples.drain(..out.len());
            self.start_us += frame_us;
        }
        if self.samples.len() < out.len() || self.start_us + frame_us > until_us {
            return false;
        }
        let n = out.len();
        for (o, s) in out.iter_mut().zip(self.samples.drain(..n)) {
            *o = s;
        }
        self.start_us += frame_us;
        true
    }
}

/// Samples captured, mono, and when the first was, on the client's clock.
struct Chunk {
    capture_us: u64,
    samples: Vec<f32>,
}

/// The open microphone; dropping it closes it.
pub struct Mic {
    _stream: cpal::Stream,
    reference: Arc<Reference>,
}

impl Drop for Mic {
    fn drop(&mut self) {
        // The stream goes next, which ends the worker's channel and so the
        // worker.
        self.reference.close();
    }
}

pub fn open(client: Arc<Client>, reference: Arc<Reference>, echo_cancel: bool) -> anyhow::Result<Mic> {
    let host = crate::audio::host();
    let device = host.default_input_device().context("no microphone")?;
    let default = device.default_input_config().context("the microphone's format")?;
    let channels = default.channels() as usize;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: MIC.sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let (tx, rx) = mpsc::sync_channel::<Chunk>(64);
    let stream = {
        let client = client.clone();
        device.build_input_stream(
            config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let ts = info.timestamp();
                let latency = ts.callback.duration_since(ts.capture);
                let capture_us = client.now_us().saturating_sub(latency.as_micros() as u64);
                let samples = data.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
                let _ = tx.try_send(Chunk { capture_us, samples });
            },
            |err| tracing::warn!(%err, "microphone"),
            None,
        )
    }
    .context("opening the microphone")?;
    stream.play().context("starting the microphone")?;
    let far = echo_cancel.then(|| reference.open());
    std::thread::Builder::new().name("farsight-mic".into()).spawn(move || worker(rx, client, far))?;
    tracing::info!(device = ?device.id().ok(), channels, echo_cancel, "microphone open");
    Ok(Mic { _stream: stream, reference })
}

fn worker(rx: Receiver<Chunk>, client: Arc<Client>, mut queue: Option<rtrb::Consumer<Piece>>) {
    let n = MIC.frame_samples();
    let frame_us = MIC.frame_us as u64;
    let mut apm = queue.is_some().then(|| {
        let config = sonora::Config {
            echo_canceller: Some(Default::default()),
            noise_suppression: Some(Default::default()),
            high_pass_filter: Some(Default::default()),
            ..Default::default()
        };
        let stream = sonora::StreamConfig::new(MIC.sample_rate, 1);
        sonora::AudioProcessing::builder().config(config).capture_config(stream).render_config(stream).build()
    });
    let mut far_end = FarEnd::default();
    let (mut pending, mut pending_us) = (VecDeque::<f32>::new(), 0u64);
    let (mut frame, mut far, mut scratch) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let (mut sent, mut far_frames, mut reported) = (0u32, 0u32, Instant::now());
    for chunk in rx {
        // The newest timestamp places what is pending; a jump means
        // capture was lost, and what is pending goes with it.
        let expected_us = pending_us + FarEnd::us(pending.len());
        if pending.is_empty() || chunk.capture_us.abs_diff(expected_us) > GAP_US {
            pending.clear();
            pending_us = chunk.capture_us;
        }
        pending.extend(&chunk.samples);
        while pending.len() >= n {
            for (f, s) in frame.iter_mut().zip(pending.drain(..n)) {
                *f = s;
            }
            let capture_us = pending_us;
            pending_us += frame_us;
            if let (Some(apm), Some(queue)) = (apm.as_mut(), queue.as_mut()) {
                while let Ok(piece) = queue.pop() {
                    far_end.add(piece.play_us, &piece.samples[..piece.len]);
                }
                while far_end.frame(capture_us + frame_us + LEAD_US, &mut far) {
                    let _ = apm.process_render_f32(&[&far], &mut [&mut scratch]);
                    far_frames += 1;
                }
                // Unused by AEC3, which finds the delay itself, but the
                // module wants it set.
                let _ = apm.set_stream_delay_ms(0);
                scratch.copy_from_slice(&frame);
                let _ = apm.process_capture_f32(&[&scratch], &mut [&mut frame]);
            }
            client.send_mic(&frame, capture_us);
            sent += 1;
        }
        if reported.elapsed() >= REPORT_EVERY {
            let stats = apm.as_ref().map(|a| a.statistics().clone());
            tracing::info!(
                frames = sent,
                far_frames,
                erle_db = ?stats.as_ref().and_then(|s| s.echo_return_loss_enhancement).map(|v| (v * 10.0).round() / 10.0),
                aec_delay_ms = ?stats.as_ref().and_then(|s| s.delay_ms),
                "microphone"
            );
            (sent, far_frames, reported) = (0, 0, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_far_end_waits_for_its_time_and_fills_gaps() {
        let mut far = FarEnd::default();
        let mut out = vec![0.0; 480];
        far.add(100_000, &[1.0; 960]);
        // Heard from 100 ms: nothing before 110 ms is due.
        assert!(!far.frame(109_000, &mut out));
        assert!(far.frame(110_000, &mut out));
        assert!(out.iter().all(|&s| s == 1.0));
        // The output underran for 10 ms: silence comes first.
        far.add(130_000, &[2.0; 480]);
        assert!(far.frame(130_000, &mut out));
        assert!(out.iter().all(|&s| s == 1.0));
        assert!(far.frame(130_000, &mut out));
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(far.frame(140_000, &mut out));
        assert!(out.iter().all(|&s| s == 2.0));
        assert!(!far.frame(1_000_000, &mut out));
    }

    #[test]
    fn a_far_end_long_past_is_dropped() {
        let mut far = FarEnd::default();
        let mut out = vec![0.0; 480];
        far.add(0, &[1.0; 48_000]);
        assert!(far.frame(900_000, &mut out));
        assert!(far.start_us + MAX_FAR_US >= 900_000);
    }
}
