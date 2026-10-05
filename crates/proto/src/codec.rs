//! Codec negotiation (`docs/design.md` §3). The client lists what it can
//! decode; the server intersects that with what its hardware can encode,
//! ranks the matches with [`rank`] and picks the first. The rest are the
//! fallback order, used when a decoder fails or a resize goes past a
//! format's limits. Tiles (§2), which every client supports, come last.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    /// Higher is more efficient: AV1 > HEVC > H.264.
    fn efficiency(self) -> u8 {
        match self {
            Codec::H264 => 0,
            Codec::Hevc => 1,
            Codec::Av1 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

/// What a video stream is encoded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Format {
    pub codec: Codec,
    pub chroma: Chroma,
    pub bit_depth: u8,
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let codec = match self.codec {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        };
        let chroma = match self.chroma {
            Chroma::Yuv420 => "420",
            Chroma::Yuv444 => "444",
        };
        write!(f, "{codec}:{chroma}")?;
        if self.bit_depth != 8 {
            write!(f, ":{}", self.bit_depth)?;
        }
        Ok(())
    }
}

/// How a stream is sent: as video, or as tiles when the server has no
/// hardware encoder for any format the client decodes (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    Video(Format),
    Tiles,
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Encoding::Video(format) => format.fmt(f),
            Encoding::Tiles => f.write_str("tiles"),
        }
    }
}

/// What the user wants most from the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Mode {
    /// Sharp text: prefer 4:4:4 (and 4:4:4 JPEG in tiles).
    #[default]
    Text,
    /// Motion: prefer the most efficient codec, in 4:2:0.
    Motion,
}

/// One decoder the client offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoderCaps {
    pub format: Format,
    pub max_width: u32,
    pub max_height: u32,
    /// Decoded by hardware rather than software.
    pub hardware: bool,
    /// The decoder accepts slices as they arrive, before the frame is whole.
    pub partial_decode: bool,
}

/// One hardware encoder the server has. Not sent; the server ranks with
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCaps {
    pub format: Format,
    pub max_width: u32,
    pub max_height: u32,
    pub hardware: bool,
}

/// A format both ends support, and the limits both share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub format: Format,
    pub max_width: u32,
    pub max_height: u32,
    /// Which of the encoders this is.
    pub encoder: usize,
    /// How many ends do it in hardware: 0, 1 or 2.
    pub hardware: u8,
}

impl Choice {
    pub fn fits(&self, width: u32, height: u32) -> bool {
        width <= self.max_width && height <= self.max_height
    }
}

/// Every format both ends support, best first:
///
/// 1. hardware on both ends, then on one;
/// 2. the mode: 4:4:4 first for text, 4:2:0 first for motion;
/// 3. codec efficiency: AV1 > HEVC > H.264.
///
/// Encoders earlier in `encoders` win ties, so the server lists its
/// preferred backend first. When one format has several encoders or
/// decoders, the best pairing is kept.
pub fn rank(encoders: &[EncoderCaps], decoders: &[DecoderCaps], mode: Mode) -> Vec<Choice> {
    let mut choices: Vec<Choice> = Vec::new();
    for (i, e) in encoders.iter().enumerate() {
        for d in decoders.iter().filter(|d| d.format == e.format) {
            let c = Choice {
                format: e.format,
                max_width: e.max_width.min(d.max_width),
                max_height: e.max_height.min(d.max_height),
                encoder: i,
                hardware: e.hardware as u8 + d.hardware as u8,
            };
            match choices.iter_mut().find(|x| x.format == c.format) {
                Some(x) if c.hardware > x.hardware => *x = c,
                Some(_) => {}
                None => choices.push(c),
            }
        }
    }
    let wants_444 = mode == Mode::Text;
    // A stable sort keeps the encoders' order among equals.
    choices.sort_by_key(|c| {
        (
            std::cmp::Reverse(c.hardware),
            (c.format.chroma == Chroma::Yuv444) != wants_444,
            std::cmp::Reverse(c.format.codec.efficiency()),
        )
    });
    choices
}

/// What every one of several clients can decode (§6: viewers share the
/// controlling client's stream): the formats all of them list, within the
/// smallest of their limits, in hardware only if every one decodes it in
/// hardware.
pub fn shared(clients: &[&[DecoderCaps]]) -> Vec<DecoderCaps> {
    let Some((first, rest)) = clients.split_first() else { return Vec::new() };
    first
        .iter()
        .filter_map(|d| {
            let mut d = d.clone();
            for other in rest {
                // The best of this client's decoders for the format.
                let o = other.iter().filter(|o| o.format == d.format).max_by_key(|o| (o.hardware, o.max_width))?;
                d.max_width = d.max_width.min(o.max_width);
                d.max_height = d.max_height.min(o.max_height);
                d.hardware &= o.hardware;
                d.partial_decode &= o.partial_decode;
            }
            Some(d)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_keeps_what_everyone_decodes() {
        let caps = |format, max, hardware| DecoderCaps { format, max_width: max, max_height: max, hardware, partial_decode: false };
        let a = [caps(H264, 4096, true), caps(HEVC, 8192, true)];
        let b = [caps(H264, 1920, false), caps(AV1, 4096, true)];
        assert_eq!(shared(&[&a, &b]), [caps(H264, 1920, false)]);
        assert_eq!(shared(&[&a]), a);
        assert!(shared(&[]).is_empty());
    }

    const fn fmt(codec: Codec, chroma: Chroma) -> Format {
        Format { codec, chroma, bit_depth: 8 }
    }
    const H264: Format = fmt(Codec::H264, Chroma::Yuv420);
    const H264_444: Format = fmt(Codec::H264, Chroma::Yuv444);
    const HEVC: Format = fmt(Codec::Hevc, Chroma::Yuv420);
    const AV1: Format = fmt(Codec::Av1, Chroma::Yuv420);

    fn enc(format: Format, hardware: bool) -> EncoderCaps {
        EncoderCaps { format, max_width: 4096, max_height: 4096, hardware }
    }
    fn dec(format: Format, hardware: bool) -> DecoderCaps {
        DecoderCaps { format, max_width: 8192, max_height: 8192, hardware, partial_decode: false }
    }
    fn formats(c: &[Choice]) -> Vec<Format> {
        c.iter().map(|c| c.format).collect()
    }

    #[test]
    fn hardware_on_both_ends_wins() {
        let e = [enc(H264, true), enc(HEVC, true), enc(AV1, false), enc(H264_444, false)];
        let d = [dec(H264, true), dec(HEVC, false), dec(AV1, true), dec(H264_444, false)];
        assert_eq!(formats(&rank(&e, &d, Mode::Text)), [H264, AV1, HEVC, H264_444]);
    }

    #[test]
    fn mode_then_efficiency() {
        let e = [enc(H264, false), enc(HEVC, false), enc(AV1, false), enc(H264_444, false)];
        let d = [dec(H264, false), dec(HEVC, false), dec(AV1, false), dec(H264_444, false)];
        assert_eq!(formats(&rank(&e, &d, Mode::Text)), [H264_444, AV1, HEVC, H264]);
        assert_eq!(formats(&rank(&e, &d, Mode::Motion)), [AV1, HEVC, H264, H264_444]);
    }

    #[test]
    fn limits_and_best_pairing() {
        let e = [enc(H264, false), enc(H264, true)];
        let d = [dec(H264, true)];
        let c = rank(&e, &d, Mode::Motion);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].encoder, c[0].hardware, c[0].max_width), (1, 2, 4096));
        assert!(c[0].fits(4096, 2160) && !c[0].fits(4097, 100));
    }

    #[test]
    fn nothing_in_common() {
        assert!(rank(&[enc(HEVC, true)], &[dec(H264, true)], Mode::Text).is_empty());
    }

    #[test]
    fn format_names() {
        assert_eq!(H264.to_string(), "h264:420");
        assert_eq!(Format { bit_depth: 10, ..H264_444 }.to_string(), "h264:444:10");
    }
}
