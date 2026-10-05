//! No video or audio off Android.

use std::sync::Arc;

use farsight_client::{Client, TilesPacket, VideoFrame};
use farsight_proto::audio::AudioConfig;
use farsight_proto::control::Epoch;

use super::MediaStats;
use crate::session::VideoDecoder;

pub struct Media;

impl Media {
    pub fn new(_decoders: Vec<VideoDecoder>) -> Self {
        Media
    }

    pub fn set_client(&self, _client: Option<Arc<Client>>) {}

    pub fn epoch(&self, _epoch: &Epoch) {}

    pub fn frame(&self, _frame: VideoFrame) {}

    pub fn tiles(&self, _tiles: TilesPacket) {}

    pub fn audio_config(&self, _config: AudioConfig) {}

    pub fn stop_audio(&self) {}

    pub fn set_mic(&self, on: bool) -> anyhow::Result<()> {
        anyhow::ensure!(!on, "no microphone here");
        Ok(())
    }

    pub fn stats(&self) -> MediaStats {
        MediaStats::default()
    }
}
