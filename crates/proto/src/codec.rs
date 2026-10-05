//! Codec negotiation types (`docs/design.md` §3). The client lists what it
//! can decode; the server intersects that with what it can encode and picks.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

/// One decoder the client offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderCaps {
    pub codec: Codec,
    pub chroma: Chroma,
    pub bit_depth: u8,
    pub max_width: u32,
    pub max_height: u32,
    /// Decoded by hardware rather than software.
    pub hardware: bool,
    /// The decoder accepts slices as they arrive, before the frame is whole.
    pub partial_decode: bool,
}
