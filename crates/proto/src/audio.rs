//! Audio datagrams (`docs/design.md` §8). Desktop audio is Opus in fixed
//! frames (5 ms by default, set by `AudioConfig`); each datagram carries the
//! newest frame and repeats the ones before it, so a lost datagram costs
//! nothing. Hand-encoded, like video fragments: 200 of them go every second.
//!
//! ```text
//! flags u8 | seq u32 | capture_us u64 | count u8 | count × (len u16, opus)
//! ```
//!
//! Frames are newest first: `seq`, `seq - 1`, …

use serde::{Deserialize, Serialize};

/// Frame `seq` is digital silence and carries no Opus: play silence, not
/// loss concealment.
pub const FLAG_SILENCE: u8 = 1 << 0;
/// The stream restarted (after idling, or for a new client): reset the
/// decoder and the jitter buffer.
pub const FLAG_DISCONTINUITY: u8 = 1 << 1;

/// Frames carried in each datagram: the newest and two repeats.
pub const REDUNDANCY: usize = 3;

pub const SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub flags: u8,
    /// The newest frame's number. Frames count up by one per frame.
    pub seq: u32,
    /// When the newest frame's first sample was captured, in server µs
    /// (the clock video's `capture_us` uses).
    pub capture_us: u64,
    /// Opus frames, newest first. An empty frame is silence.
    pub frames: Vec<&'a [u8]>,
}

impl<'a> AudioPacket<'a> {
    const HEADER: usize = 1 + 4 + 8 + 1;

    pub fn silence(&self) -> bool {
        self.flags & FLAG_SILENCE != 0
    }

    pub fn discontinuity(&self) -> bool {
        self.flags & FLAG_DISCONTINUITY != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(Self::HEADER + self.frames.iter().map(|f| 2 + f.len()).sum::<usize>());
        out.push(self.flags);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.push(self.frames.len() as u8);
        for f in &self.frames {
            out.extend_from_slice(&(f.len() as u16).to_le_bytes());
            out.extend_from_slice(f);
        }
    }

    pub fn read(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < Self::HEADER {
            return None;
        }
        let flags = buf[0];
        let seq = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        let capture_us = u64::from_le_bytes(buf[5..13].try_into().unwrap());
        let count = buf[13] as usize;
        let mut rest = &buf[Self::HEADER..];
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            if rest.len() < 2 {
                return None;
            }
            let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            let frame = rest.get(2..2 + len)?;
            frames.push(frame);
            rest = &rest[2 + len..];
        }
        Some(Self { flags, seq, capture_us, frames })
    }
}

/// What a client can play, in `Hello`. `None` there means no audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCaps {
    pub max_channels: u8,
}

/// The stream's format, sent before its first datagram and whenever it
/// changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    pub channels: u8,
    pub sample_rate: u32,
    /// Each frame's length, in µs.
    pub frame_us: u32,
}

impl AudioConfig {
    pub fn frame_samples(&self) -> usize {
        (self.sample_rate as u64 * self.frame_us as u64 / 1_000_000) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let p = AudioPacket {
            flags: FLAG_DISCONTINUITY,
            seq: 77,
            capture_us: 1 << 33,
            frames: vec![b"newest", b"", b"oldest"],
        };
        let mut buf = Vec::new();
        p.write(&mut buf);
        assert_eq!(AudioPacket::read(&buf), Some(p));
        assert!(AudioPacket::read(&buf[..buf.len() - 1]).is_none());
    }

    #[test]
    fn frame_samples() {
        let c = AudioConfig { channels: 2, sample_rate: SAMPLE_RATE, frame_us: 5000 };
        assert_eq!(c.frame_samples(), 240);
    }
}
