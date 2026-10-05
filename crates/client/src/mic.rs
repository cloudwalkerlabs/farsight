//! The client's microphone, on its way to the session (`docs/design.md`
//! §8): mono 10 ms frames of Opus for voice, with in-band FEC, each
//! datagram repeating the frame before its own. Echo cancellation is the
//! app's: it hands over what its microphone heard with the session's
//! audio taken out.

use std::collections::VecDeque;

use farsight_proto::audio::{AudioPacket, FLAG_DISCONTINUITY, MIC, MIC_REPEATS};
use farsight_proto::datagram::Datagram;

/// Voice needs little: 32 kbit/s is wideband SILK with room for FEC.
const BITRATE: i32 = 32_000;

/// The loss in-band FEC is sized for, in percent.
const EXPECTED_LOSS: i32 = 10;

pub struct MicEncoder {
    encoder: opus::Encoder,
    /// Recent frames, newest first.
    recent: VecDeque<Vec<u8>>,
    seq: u32,
    discontinuity: bool,
}

impl MicEncoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(MIC.sample_rate, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(EXPECTED_LOSS)?;
        Ok(Self { encoder, recent: VecDeque::new(), seq: 0, discontinuity: true })
    }

    /// Encodes one frame ([`MIC`]'s samples) captured at `capture_us` on
    /// the server's clock, and returns its datagram.
    pub fn encode(&mut self, pcm: &[f32], capture_us: u64) -> Option<Vec<u8>> {
        let mut out = vec![0; 1500];
        let n = match self.encoder.encode_float(pcm, &mut out) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(%err, "opus");
                return None;
            }
        };
        out.truncate(n);
        self.recent.push_front(out);
        self.recent.truncate(1 + MIC_REPEATS);
        self.seq = self.seq.wrapping_add(1);
        let flags = if std::mem::take(&mut self.discontinuity) { FLAG_DISCONTINUITY } else { 0 };
        let frames = self.recent.iter().map(Vec::as_slice).collect();
        Some(Datagram::Mic(AudioPacket { flags, seq: self.seq, capture_us, frames }).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_repeat_and_the_first_starts_a_stream() {
        let mut m = MicEncoder::new().unwrap();
        let pcm = vec![0.1; MIC.frame_samples()];
        let a = m.encode(&pcm, 0).unwrap();
        let b = m.encode(&pcm, 10_000).unwrap();
        let Some(Datagram::Mic(a)) = Datagram::decode(&a) else { panic!() };
        let Some(Datagram::Mic(b)) = Datagram::decode(&b) else { panic!() };
        assert!(a.discontinuity() && !b.discontinuity());
        assert_eq!((a.frames.len(), b.frames.len()), (1, 1 + MIC_REPEATS));
        assert_eq!(b.seq, a.seq + 1);
        assert_eq!(b.frames[1], a.frames[0]);
    }
}
