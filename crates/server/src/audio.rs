//! The session's speaker (docs/design.md §8): a PipeWire stream that is an
//! `Audio/Sink` node, `farsight-speaker`. WirePlumber makes it the
//! session's default sink, so every app plays into it. Each graph cycle is
//! one 5 ms frame, which goes out as Opus with the two frames before it
//! repeated.
//!
//! Two threads: PipeWire's (its realtime `process` callback only copies the
//! cycle's samples into a bounded channel) and the encoder's. Nothing is
//! encoded while no client listens. The node only runs while something
//! plays; digital silence goes out as a few flagged datagrams and then
//! nothing, until sound starts again.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use farsight_proto::audio::{
    AudioConfig, AudioPacket, FLAG_DISCONTINUITY, FLAG_SILENCE, REDUNDANCY, SAMPLE_RATE,
};
use farsight_proto::datagram::Datagram;
use pipewire as pw;
use pw::spa;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Audio, ToNet};

pub const CHANNELS: usize = 2;

/// The stream's format: stereo, 5 ms frames.
pub const CONFIG: AudioConfig = AudioConfig { channels: CHANNELS as u8, sample_rate: SAMPLE_RATE, frame_us: 5000 };

/// 128 kbit/s for stereo, as §8 plans.
const BITRATE: i32 = 128_000;

/// Silent frames still sent once sound stops, so the client hears the
/// silence rather than concealing loss; then nothing until sound again.
const SILENT_FRAMES: u32 = 20;

/// Cycles waiting for the encoder; beyond this they are dropped.
const QUEUE: usize = 16;

/// The largest graph cycle we take whole, in samples per channel.
const MAX_CYCLE: usize = 2048;

/// One graph cycle's samples, interleaved.
struct Cycle {
    capture_us: u64,
    frames: usize,
    samples: Box<[f32; MAX_CYCLE * CHANNELS]>,
}

/// Starts the speaker on the session's PipeWire at `socket`. Datagrams go
/// to `net` while a client listens.
pub fn spawn(socket: PathBuf, start: Instant, net: UnboundedSender<ToNet>, shared: Arc<Audio>) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Cycle>(QUEUE);
    // Buffers go back and forth, so the realtime thread never allocates.
    let (free_tx, free_rx) = mpsc::sync_channel::<Box<[f32; MAX_CYCLE * CHANNELS]>>(QUEUE + 2);
    for _ in 0..QUEUE + 2 {
        let _ = free_tx.try_send(Box::new([0.0; MAX_CYCLE * CHANNELS]));
    }
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new().name("farsight-pipewire".into()).spawn(move || {
        if let Err(err) = pipewire_thread(socket, start, tx, free_rx, &ready_tx) {
            let _ = ready_tx.send(Err(err));
        }
    })?;
    ready_rx.recv().context("the PipeWire thread ended")??;
    let _ = shared.config.set(CONFIG);
    std::thread::Builder::new()
        .name("farsight-opus".into())
        .spawn(move || encode_thread(rx, free_tx, net, shared))?;
    Ok(())
}

fn pipewire_thread(
    socket: PathBuf,
    start: Instant,
    tx: SyncSender<Cycle>,
    free: Receiver<Box<[f32; MAX_CYCLE * CHANNELS]>>,
    ready: &mpsc::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(Some(pw::properties::properties! {
            *pw::keys::REMOTE_NAME => socket.to_string_lossy().into_owned(),
        }))
        .with_context(|| format!("connecting to PipeWire at {}", socket.display()))?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => "farsight-speaker",
        *pw::keys::NODE_DESCRIPTION => "farsight client",
    };
    let stream = pw::stream::StreamBox::new(&core, "farsight-speaker", props)?;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| tracing::debug!(?old, ?new, "speaker state"))
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            let bytes = &bytes[..size.min(bytes.len())];
            let frames = (bytes.len() / (4 * CHANNELS)).min(MAX_CYCLE);
            if frames == 0 {
                return;
            }
            let Ok(mut samples) = free.try_recv() else { return }; // encoder behind
            for (s, b) in samples.iter_mut().zip(bytes[..frames * CHANNELS * 4].as_chunks::<4>().0) {
                *s = f32::from_le_bytes(*b);
            }
            let capture_us = start.elapsed().as_micros() as u64;
            let _ = tx.try_send(Cycle { capture_us, frames, samples });
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).context("format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    tracing::info!(socket = %socket.display(), "farsight-speaker is the session's sink");
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// Turns cycles into 5 ms Opus frames and datagrams.
struct Packer {
    encoder: opus::Encoder,
    /// Samples not yet framed, interleaved, and the capture time of the
    /// first.
    pending: Vec<f32>,
    pending_us: u64,
    /// Recent frames, newest first, for redundancy.
    recent: Vec<Vec<u8>>,
    seq: u32,
    /// The next frame starts a new stream.
    discontinuity: bool,
    silent_run: u32,
    /// The expected capture time of the next cycle, to notice gaps.
    next_us: Option<u64>,
}

impl Packer {
    fn new() -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        Ok(Self {
            encoder,
            pending: Vec::new(),
            pending_us: 0,
            recent: Vec::new(),
            seq: 0,
            discontinuity: true,
            silent_run: 0,
            next_us: None,
        })
    }

    fn restart(&mut self) {
        self.discontinuity = true;
        self.pending.clear();
        self.recent.clear();
        self.silent_run = 0;
        let _ = self.encoder.reset_state();
    }

    /// Takes one cycle, and calls `send` with each datagram it completes.
    fn push(&mut self, cycle: &Cycle, mut send: impl FnMut(Vec<u8>)) {
        let frame_samples = CONFIG.frame_samples();
        let cycle_us = cycle.frames as u64 * 1_000_000 / SAMPLE_RATE as u64;
        // The node paused and resumed: what follows is a new stream.
        if self.next_us.is_some_and(|n| cycle.capture_us > n + 2 * CONFIG.frame_us as u64) {
            self.restart();
        }
        self.next_us = Some(cycle.capture_us + cycle_us);
        if self.pending.is_empty() {
            self.pending_us = cycle.capture_us;
        }
        self.pending.extend_from_slice(&cycle.samples[..cycle.frames * CHANNELS]);
        while self.pending.len() >= frame_samples * CHANNELS {
            let frame: Vec<f32> = self.pending.drain(..frame_samples * CHANNELS).collect();
            let capture_us = self.pending_us;
            self.pending_us += CONFIG.frame_us as u64;
            if let Some(d) = self.frame(&frame, capture_us) {
                send(d);
            }
        }
    }

    fn frame(&mut self, samples: &[f32], capture_us: u64) -> Option<Vec<u8>> {
        let silent = samples.iter().all(|&s| s == 0.0);
        if silent {
            self.silent_run += 1;
            if self.silent_run > SILENT_FRAMES {
                // Quiet: send nothing; sound after this is a new stream.
                self.discontinuity = true;
                self.recent.clear();
                return None;
            }
        } else {
            self.silent_run = 0;
        }
        let encoded = if silent {
            Vec::new()
        } else {
            let mut out = vec![0; 1500];
            match self.encoder.encode_float(samples, &mut out) {
                Ok(n) => {
                    out.truncate(n);
                    out
                }
                Err(err) => {
                    tracing::warn!(%err, "opus");
                    return None;
                }
            }
        };
        if self.discontinuity {
            self.recent.clear();
        }
        self.recent.insert(0, encoded);
        self.recent.truncate(REDUNDANCY);
        self.seq = self.seq.wrapping_add(1);
        let mut flags = 0;
        if silent {
            flags |= FLAG_SILENCE;
        }
        if std::mem::take(&mut self.discontinuity) {
            flags |= FLAG_DISCONTINUITY;
        }
        let packet = AudioPacket {
            flags,
            seq: self.seq,
            capture_us,
            frames: self.recent.iter().map(Vec::as_slice).collect(),
        };
        Some(Datagram::Audio(packet).to_vec())
    }
}

fn encode_thread(
    rx: Receiver<Cycle>,
    free: SyncSender<Box<[f32; MAX_CYCLE * CHANNELS]>>,
    net: UnboundedSender<ToNet>,
    shared: Arc<Audio>,
) {
    let mut packer = match Packer::new() {
        Ok(p) => p,
        Err(err) => {
            tracing::error!("opus: {err:#}");
            return;
        }
    };
    let mut was_listening = false;
    for cycle in rx {
        let now_listening = shared.listening.load(Ordering::Relaxed);
        if now_listening && !was_listening {
            packer.restart();
        }
        was_listening = now_listening;
        if now_listening {
            packer.push(&cycle, |d| {
                let _ = net.send(ToNet::Audio(Bytes::from(d)));
            });
        }
        let _ = free.try_send(cycle.samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(us: u64, value: f32) -> Cycle {
        Cycle { capture_us: us, frames: 240, samples: Box::new([value; MAX_CYCLE * CHANNELS]) }
    }

    #[test]
    fn frames_carry_redundancy_and_silence_stops() {
        let mut p = Packer::new().unwrap();
        let mut out = Vec::new();
        for i in 0..4 {
            p.push(&cycle(i * 5000, 0.1), |d| out.push(d));
        }
        assert_eq!(out.len(), 4);
        let Some(Datagram::Audio(first)) = Datagram::decode(&out[0]) else { panic!() };
        assert!(first.discontinuity());
        assert_eq!(first.frames.len(), 1);
        let Some(Datagram::Audio(last)) = Datagram::decode(&out[3]) else { panic!() };
        assert_eq!((last.seq, last.frames.len(), last.capture_us), (first.seq + 3, REDUNDANCY, 15_000));
        assert!(!last.discontinuity());

        out.clear();
        for i in 4..4 + SILENT_FRAMES as u64 + 10 {
            p.push(&cycle(i * 5000, 0.0), |d| out.push(d));
        }
        assert_eq!(out.len(), SILENT_FRAMES as usize);
        p.push(&cycle(200_000, 0.1), |d| out.push(d));
        let Some(Datagram::Audio(again)) = Datagram::decode(out.last().unwrap()) else { panic!() };
        assert!(again.discontinuity());
    }

    #[test]
    fn a_gap_restarts_the_stream() {
        let mut p = Packer::new().unwrap();
        let mut out = Vec::new();
        p.push(&cycle(0, 0.1), |d| out.push(d));
        p.push(&cycle(5000, 0.1), |d| out.push(d));
        p.push(&cycle(1_000_000, 0.1), |d| out.push(d));
        let Some(Datagram::Audio(d)) = Datagram::decode(&out[2]) else { panic!() };
        assert!(d.discontinuity());
        assert_eq!(d.frames.len(), 1);
    }
}
