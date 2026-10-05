//! The farsight wire protocol: the messages and packet headers that the
//! server and clients exchange. Encoding and decoding only; no I/O.

pub mod codec;
pub mod layout;

/// ALPN token for the QUIC connection. Bumped on incompatible changes.
pub const ALPN: &[u8] = b"farsight/0";

/// UDP port the server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7740;
