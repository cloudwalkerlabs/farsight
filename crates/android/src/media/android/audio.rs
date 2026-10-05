//! The session's audio on AAudio (§8): an output stream that pulls from the
//! client core's jitter buffer, and, while the session wants it, the
//! microphone.
//!
//! Echo cancellation is the platform's: the microphone opens with the
//! `VOICE_COMMUNICATION` preset, and while it is open the output plays as
//! voice communication too, with the app in communication mode (Kotlin's
//! part), so the device's canceller has the far end it needs. Without the
//! microphone, the output plays as media, in low-latency mode.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use farsight_client::Client;
use farsight_proto::audio::{AudioConfig, MIC};
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage, Clockid,
};

/// Samples in one piece of the microphone, on its way to the worker.
const PIECE: usize = 1024;

/// Pieces queued: about 1.4 s, while the worker is behind.
const PIECES: usize = 64;

/// The connection the callbacks read from; it changes on reconnecting.
pub type ClientSlot = Arc<Mutex<Option<Arc<Client>>>>;

pub struct Audio {
    client: ClientSlot,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    config: Option<AudioConfig>,
    output: Option<Stream>,
    mic: Option<Stream>,
}

/// An open stream; closed when dropped.
struct Stream(#[allow(dead_code)] AudioStream);

// SAFETY: AAudio streams may be started, stopped and closed from any
// thread; these are only touched under the state's lock.
unsafe impl Send for Stream {}

impl Audio {
    pub fn new(client: ClientSlot) -> Self {
        Audio { client, state: Mutex::default() }
    }

    /// The session's audio starts, or changes format.
    pub fn start(&self, config: AudioConfig) {
        let mut s = self.state.lock().unwrap();
        s.config = Some(config);
        let voice = s.mic.is_some();
        s.output = None;
        match open_output(&self.client, config, voice) {
            Ok(stream) => s.output = Some(Stream(stream)),
            Err(err) => {
                tracing::warn!("{err:#}; muting the session's audio");
                if let Some(c) = self.client.lock().unwrap().as_ref() {
                    c.set_audio(false);
                }
            }
        }
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        s.output = None;
        s.mic = None;
        s.config = None;
    }

    pub fn mic_on(&self) -> bool {
        self.state.lock().unwrap().mic.is_some()
    }

    /// Opens or closes the microphone; the output follows, as voice or as
    /// media.
    pub fn set_mic(&self, on: bool) -> anyhow::Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.mic.is_some() == on {
            return Ok(());
        }
        // Close first: the output's usage changes with it.
        s.output = None;
        s.mic = None;
        let result = if on { open_mic(&self.client).map(|m| s.mic = Some(Stream(m))) } else { Ok(()) };
        if let Some(config) = s.config {
            let voice = s.mic.is_some();
            match open_output(&self.client, config, voice) {
                Ok(stream) => s.output = Some(Stream(stream)),
                Err(err) => tracing::warn!("{err:#}"),
            }
        }
        tracing::info!(on = s.mic.is_some(), "microphone");
        result
    }
}

fn now_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// How long until what is written now is heard, in µs.
fn output_delay_us(stream: &AudioStream, rate: i64) -> u64 {
    match stream.timestamp(Clockid::Monotonic) {
        Ok(ts) => {
            let ahead = stream.frames_written() - ts.frame_position;
            let heard_ns = ts.time_nanoseconds + ahead * 1_000_000_000 / rate;
            (heard_ns - now_ns()).max(0) as u64 / 1000
        }
        // Not running yet: what is buffered.
        Err(_) => stream.buffer_size_in_frames().max(0) as u64 * 1_000_000 / rate as u64,
    }
}

fn open_output(client: &ClientSlot, config: AudioConfig, voice: bool) -> anyhow::Result<AudioStream> {
    let channels = config.channels as usize;
    let rate = config.sample_rate as i64;
    let client = client.clone();
    let callback = move |stream: &AudioStream, data: *mut std::ffi::c_void, frames: i32| {
        // SAFETY: AAudio hands a buffer of `frames` frames of floats.
        let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, frames as usize * channels) };
        let delay = output_delay_us(stream, rate);
        match client.lock().unwrap().as_ref() {
            Some(c) => c.fill_audio(out, delay),
            None => out.fill(0.0),
        }
        AudioCallbackResult::Continue
    };
    let (usage, content) =
        if voice { (AudioUsage::VoiceCommunication, AudioContentType::Speech) } else { (AudioUsage::Media, AudioContentType::Music) };
    let stream = AudioStreamBuilder::new()?
        .direction(AudioDirection::Output)
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .format(AudioFormat::PCM_Float)
        .channel_count(config.channels as i32)
        .sample_rate(config.sample_rate as i32)
        .usage(usage)
        .content_type(content)
        .data_callback(Box::new(callback))
        .error_callback(Box::new(|_, err| tracing::warn!(?err, "audio output")))
        .open_stream()
        .context("opening the audio output")?;
    stream.request_start().context("starting the audio output")?;
    tracing::info!(
        voice,
        burst = stream.frames_per_burst(),
        buffer = stream.buffer_size_in_frames(),
        mode = ?stream.performance_mode(),
        "playing audio"
    );
    Ok(stream)
}

/// Some of what the microphone heard, mono.
struct Piece {
    /// When the first sample was captured, on the client's clock.
    capture_us: u64,
    len: usize,
    samples: [f32; PIECE],
}

fn open_mic(client: &ClientSlot) -> anyhow::Result<AudioStream> {
    let (mut producer, consumer) = rtrb::RingBuffer::<Piece>::new(PIECES);
    let rate = MIC.sample_rate as i64;
    let slot = client.clone();
    let callback = move |stream: &AudioStream, data: *mut std::ffi::c_void, frames: i32| {
        // SAFETY: AAudio hands `frames` mono floats.
        let input = unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize) };
        let Some(client) = slot.lock().unwrap().clone() else { return AudioCallbackResult::Continue };
        // When this callback's first frame was captured.
        let age_us = match stream.timestamp(Clockid::Monotonic) {
            Ok(ts) => {
                let at_ns = ts.time_nanoseconds + (stream.frames_read() - ts.frame_position) * 1_000_000_000 / rate;
                (now_ns() - at_ns).max(0) as u64 / 1000
            }
            Err(_) => input.len() as u64 * 1_000_000 / rate as u64,
        };
        let start_us = client.now_us().saturating_sub(age_us);
        for (i, chunk) in input.chunks(PIECE).enumerate() {
            let mut piece = Piece {
                capture_us: start_us + (i * PIECE) as u64 * 1_000_000 / rate as u64,
                len: chunk.len(),
                samples: [0.0; PIECE],
            };
            piece.samples[..chunk.len()].copy_from_slice(chunk);
            let _ = producer.push(piece);
        }
        AudioCallbackResult::Continue
    };
    let stream = AudioStreamBuilder::new()?
        .direction(AudioDirection::Input)
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .format(AudioFormat::PCM_Float)
        .channel_count(1)
        .sample_rate(MIC.sample_rate as i32)
        .input_preset(AudioInputPreset::VoiceCommunication)
        .data_callback(Box::new(callback))
        .error_callback(Box::new(|_, err| tracing::warn!(?err, "microphone")))
        .open_stream()
        .context("opening the microphone")?;
    stream.request_start().context("starting the microphone")?;
    let worker = client.clone();
    std::thread::Builder::new().name("farsight-mic".into()).spawn(move || send(consumer, worker))?;
    tracing::info!(burst = stream.frames_per_burst(), mode = ?stream.performance_mode(), "microphone open");
    Ok(stream)
}

/// Frames the microphone into 10 ms and hands them to the core, until the
/// stream (and with it the queue's producer) is gone.
fn send(mut queue: rtrb::Consumer<Piece>, client: ClientSlot) {
    let n = MIC.frame_samples();
    let (mut pending, mut pending_us) = (Vec::<f32>::with_capacity(4 * n), 0u64);
    loop {
        let Ok(piece) = queue.pop() else {
            if queue.is_abandoned() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        if pending.is_empty() {
            pending_us = piece.capture_us;
        }
        pending.extend_from_slice(&piece.samples[..piece.len]);
        while pending.len() >= n {
            if let Some(c) = client.lock().unwrap().as_ref() {
                c.send_mic(&pending[..n], pending_us);
            }
            pending.drain(..n);
            pending_us += MIC.frame_us as u64;
        }
    }
}
