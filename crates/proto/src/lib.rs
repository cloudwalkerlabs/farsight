//! The farsight wire protocol: the messages and packet headers that the
//! server and clients exchange. Encoding and decoding only; no I/O.

pub mod audio;
pub mod codec;
pub mod control;
pub mod datagram;
pub mod input;
pub mod layout;
pub mod tiles;
pub mod video;

/// ALPN token for the QUIC connection. Bumped on incompatible changes.
pub const ALPN: &[u8] = b"farsight/2";

/// UDP port the server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7740;
