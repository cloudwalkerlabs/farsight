//! The session's audio, played through the desktop's sound server (cpal,
//! PipeWire first on Linux). The output asks for 5 ms at a time where it
//! can; the jitter buffer and drift correction are the client core's
//! (`farsight_client::audio`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use farsight_client::Client;
use farsight_proto::audio::AudioConfig;

/// How often audio statistics are logged.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// The open output; dropping it stops playback.
pub struct Output {
    _stream: cpal::Stream,
}

pub fn host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if cpal::available_hosts().contains(&cpal::HostId::PipeWire)
        && let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire)
    {
        return host;
    }
    cpal::default_host()
}

/// Opens the output; what it plays is copied to `reference` for the
/// microphone's echo canceller.
pub fn open(client: Arc<Client>, config: AudioConfig, reference: Arc<crate::mic::Reference>) -> anyhow::Result<Output> {
    let host = host();
    let device = host.default_output_device().context("no audio output device")?;
    let frame = config.frame_samples() as u32;
    let mut stream_config = cpal::StreamConfig {
        channels: config.channels as u16,
        sample_rate: config.sample_rate,
        buffer_size: cpal::BufferSize::Fixed(frame),
    };
    let build = |stream_config: cpal::StreamConfig| {
        let (client, reference) = (client.clone(), reference.clone());
        let channels = config.channels as usize;
        device.build_output_stream(
            stream_config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let delay = ts.playback.duration_since(ts.callback);
                client.fill_audio(data, delay.as_micros() as u64);
                reference.push(data, channels, client.now_us() + delay.as_micros() as u64);
            },
            |err| tracing::warn!(%err, "audio output"),
            None,
        )
    };
    let stream = match build(stream_config) {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(%err, "a {frame}-sample buffer; trying the device's default");
            stream_config.buffer_size = cpal::BufferSize::Default;
            build(stream_config).context("opening the audio output")?
        }
    };
    stream.play().context("starting the audio output")?;
    tracing::info!(host = ?host.id(), device = ?device.id().ok(), ?config, "playing audio");
    Ok(Output { _stream: stream })
}

/// Logs audio latency and the buffer's health every few seconds, while
/// the connection lasts.
pub async fn report(client: Arc<Client>) {
    let mut tick = tokio::time::interval(REPORT_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        let Some(mut s) = client.audio_stats() else { continue };
        if s.latency_us.is_empty() {
            continue;
        }
        s.latency_us.sort_unstable();
        let pct = |p: usize| s.latency_us[(s.latency_us.len() * p / 100).min(s.latency_us.len() - 1)] as f64 / 1000.0;
        tracing::info!(
            "audio: capture→speaker ms p50 {:.1} p95 {:.1} (output {:.1}); buffer {:.1} ms (target {:.1}); speed {:+.3}%; concealed {} (late {}) silent {} underruns {} skipped {}",
            pct(50),
            pct(95),
            s.output_delay_us as f64 / 1000.0,
            s.buffered_us as f64 / 1000.0,
            s.target_us as f64 / 1000.0,
            s.adjust * 100.0,
            s.concealed,
            s.late,
            s.silent,
            s.underruns,
            s.skipped,
        );
    }
}
