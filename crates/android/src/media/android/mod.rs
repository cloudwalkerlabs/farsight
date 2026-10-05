//! Video into the session view's surface, and audio on AAudio.

mod audio;
mod codec;
mod surface;
mod video;

use std::sync::{Arc, Mutex};

use farsight_client::{Client, TilesPacket, VideoFrame};
use farsight_proto::audio::AudioConfig;
use farsight_proto::control::Epoch;

use super::MediaStats;
use crate::session::VideoDecoder;

pub struct Media {
    video: video::Video,
    audio: audio::Audio,
    client: audio::ClientSlot,
}

impl Media {
    pub fn new(decoders: Vec<VideoDecoder>) -> Self {
        let client: audio::ClientSlot = Arc::new(Mutex::new(None));
        Media { video: video::Video::spawn(decoders), audio: audio::Audio::new(client.clone()), client }
    }

    pub fn set_client(&self, client: Option<Arc<Client>>) {
        *self.client.lock().unwrap() = client.clone();
        self.video.send(video::Msg::Client(client));
    }

    pub fn epoch(&self, e: &Epoch) {
        self.video.send(video::Msg::Epoch { encoding: e.encoding, size: (e.layout.width_px, e.layout.height_px) });
    }

    pub fn frame(&self, frame: VideoFrame) {
        self.video.send(video::Msg::Frame(frame));
    }

    pub fn tiles(&self, tiles: TilesPacket) {
        self.video.send(video::Msg::Tiles(tiles));
    }

    pub fn audio_config(&self, config: AudioConfig) {
        self.audio.start(config);
    }

    pub fn stop_audio(&self) {
        self.audio.stop();
    }

    pub fn set_mic(&self, on: bool, echo_cancel: bool) -> anyhow::Result<()> {
        self.audio.set_mic(on, echo_cancel)
    }

    pub fn stats(&self) -> MediaStats {
        let mut s = self.video.take_samples();
        let median = |v: &mut Vec<u64>| {
            v.sort_unstable();
            v.get(v.len() / 2).map_or(0.0, |&us| us as f32 / 1000.0)
        };
        MediaStats {
            encoding: s.encoding.clone(),
            fps: s.shown as f32,
            network_ms: median(&mut s.network_us),
            decode_ms: median(&mut s.decode_us),
            total_ms: median(&mut s.total_us),
            mic_on: self.audio.mic_on(),
        }
    }
}
