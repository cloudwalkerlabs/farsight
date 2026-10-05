//! Transport shared by the server and clients: QUIC with unreliable
//! datagrams for media and input, streams for control, plus frame
//! packetization, FEC and congestion control (`docs/design.md` §1–2).

pub mod endpoint;
pub mod packetize;
pub mod stream;

pub use quinn;
