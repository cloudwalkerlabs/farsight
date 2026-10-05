//! Messages on the reliable control stream. The client opens one
//! bidirectional stream right after connecting and sends `Hello` first.
//! Each message is a little-endian `u32` length followed by its postcard
//! encoding (see [`encode_framed`]).

use serde::{Deserialize, Serialize};

use crate::codec::{DecoderCaps, Encoding, Format, Mode};
use crate::layout::Layout;

/// The largest control message accepted, in bytes. A 256×256 cursor image
/// is the biggest thing sent today.
pub const MAX_MESSAGE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello(Hello),
    /// The window changed size or scale (§5). The server answers with an
    /// `Epoch` once a frame at the new layout is ready.
    SetLayout(Layout),
    /// The decoder lost its reference: a frame was lost or damaged.
    RequestKeyframe,
    /// Tiles only: part of an update was lost; send these regions again.
    RequestRefresh(Vec<crate::tiles::Rect>),
    /// The user switched between text and motion (§3); the server may pick
    /// another format.
    SetMode(Mode),
    /// The decoder for this format failed and can't be used again. The
    /// server moves on to the next format.
    DecoderFailed(Format),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub decoders: Vec<DecoderCaps>,
    pub layout: Layout,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome(Welcome),
    /// A new video epoch starts (§5): every frame tagged with this epoch is
    /// in this encoding and layout, starting with a keyframe (for video) or
    /// with the whole screen (for tiles). Sent before the
    /// epoch's first frame, but datagrams can overtake it.
    Epoch(Epoch),
    /// A cursor image, sent once per id before any `Cursor` names it.
    CursorImage(CursorImage),
    Cursor(CursorShape),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    /// Every encoding both ends support, best first, ending with tiles.
    /// The stream starts in the first; the rest are the fallback order
    /// (§3).
    pub encodings: Vec<Encoding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epoch {
    pub epoch: u16,
    pub encoding: Encoding,
    /// The layout in effect, which may differ from the one asked for: the
    /// server keeps sizes even and within the format's limits. The client
    /// scales the picture only then.
    pub layout: Layout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorImage {
    /// A hash of the image, so the client can cache it.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// The hotspot, in image pixels.
    pub hotspot: (i32, i32),
    /// Image pixels per output pixel, in 1/120 steps: 240 for an image
    /// drawn at twice the output's density.
    pub scale_120: u32,
    /// Premultiplied ARGB8888, little-endian (B, G, R, A in memory), rows
    /// packed.
    pub pixels: Vec<u8>,
}

/// What the cursor looks like over the remote desktop. The client draws it
/// (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CursorShape {
    Hidden,
    /// A named cursor from the client's own theme (the CSS cursor names, as
    /// in `wp_cursor_shape_v1`).
    Named(String),
    /// A `CursorImage` sent earlier.
    Image(u64),
}

#[derive(Debug)]
pub enum Error {
    TooLarge(usize),
    Decode(postcard::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TooLarge(n) => write!(f, "control message of {n} bytes is too large"),
            Error::Decode(e) => write!(f, "bad control message: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The message with its length prefix, ready to write to the stream.
pub fn encode_framed<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut out = vec![0; 4];
    out = postcard::to_extend(msg, out).expect("Vec never fails to grow");
    let len = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    out
}

/// The body length from a 4-byte prefix.
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, Error> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_MESSAGE {
        return Err(Error::TooLarge(len));
    }
    Ok(len)
}

pub fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, Error> {
    postcard::from_bytes(body).map_err(Error::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Chroma, Codec};

    #[test]
    fn framed_round_trip() {
        let msg = ClientMessage::Hello(Hello {
            decoders: vec![DecoderCaps {
                format: Format { codec: Codec::H264, chroma: Chroma::Yuv420, bit_depth: 8 },
                max_width: 4096,
                max_height: 4096,
                hardware: true,
                partial_decode: false,
            }],
            layout: Layout { width_px: 1920, height_px: 1080, scale_120: 120, refresh_mhz: 60_000 },
            mode: Mode::Text,
        });
        let bytes = encode_framed(&msg);
        let len = frame_len(bytes[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(decode::<ClientMessage>(&bytes[4..]).unwrap(), msg);
    }

    #[test]
    fn oversized_frames_are_refused() {
        assert!(frame_len((MAX_MESSAGE as u32 + 1).to_le_bytes()).is_err());
    }
}
