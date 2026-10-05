//! Unreliable datagrams: one tag byte, then the body. Video fragments use
//! the fixed [`FragmentHeader`]; the rest are postcard-encoded.

use serde::{Deserialize, Serialize};

use crate::input::InputPacket;
use crate::video::FragmentHeader;

const TAG_VIDEO: u8 = 1;
const TAG_INPUT: u8 = 2;
const TAG_PING: u8 = 3;
const TAG_PONG: u8 = 4;

/// Bytes in front of a video fragment's payload.
pub const VIDEO_OVERHEAD: usize = 1 + FragmentHeader::LEN;

/// Clock sync: the client sends a ping, the server echoes it with its own
/// time. The client estimates the server's clock offset from the
/// round trip, to measure glass-to-glass latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub client_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub client_us: u64,
    pub server_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Datagram<'a> {
    Video(FragmentHeader, &'a [u8]),
    Input(InputPacket),
    Ping(Ping),
    Pong(Pong),
}

impl<'a> Datagram<'a> {
    /// `None` for anything malformed or unknown; datagrams are dropped, not
    /// errors.
    pub fn decode(buf: &'a [u8]) -> Option<Self> {
        let (&tag, body) = buf.split_first()?;
        Some(match tag {
            TAG_VIDEO => {
                let (h, payload) = FragmentHeader::read(body)?;
                Datagram::Video(h, payload)
            }
            TAG_INPUT => Datagram::Input(postcard::from_bytes(body).ok()?),
            TAG_PING => Datagram::Ping(postcard::from_bytes(body).ok()?),
            TAG_PONG => Datagram::Pong(postcard::from_bytes(body).ok()?),
            _ => return None,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Datagram::Video(h, payload) => encode_video(h, payload, out),
            Datagram::Input(p) => tagged(TAG_INPUT, p, out),
            Datagram::Ping(p) => tagged(TAG_PING, p, out),
            Datagram::Pong(p) => tagged(TAG_PONG, p, out),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

pub fn encode_video(h: &FragmentHeader, payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(VIDEO_OVERHEAD + payload.len());
    out.push(TAG_VIDEO);
    h.write(out);
    out.extend_from_slice(payload);
}

fn tagged<T: Serialize>(tag: u8, body: &T, out: &mut Vec<u8>) {
    out.push(tag);
    let buf = std::mem::take(out);
    *out = postcard::to_extend(body, buf).expect("Vec never fails to grow");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputEvent, Snapshot};

    #[test]
    fn round_trips() {
        let h = FragmentHeader { flags: 1, epoch: 1, frame: 2, index: 0, count: 1, capture_us: 3, encode_us: 4 };
        let input = InputPacket {
            seq: 9,
            events: vec![InputEvent::Key { code: 30, pressed: true }],
            snapshot: Some(Snapshot { keys: vec![30], buttons: vec![], pointer: Some((1.5, 2.0)) }),
        };
        for d in [
            Datagram::Video(h, b"abc"),
            Datagram::Input(input),
            Datagram::Ping(Ping { client_us: 5 }),
            Datagram::Pong(Pong { client_us: 5, server_us: 6 }),
        ] {
            assert_eq!(Datagram::decode(&d.to_vec()), Some(d));
        }
    }

    #[test]
    fn full_input_packet_fits_a_datagram() {
        let mut tx = crate::input::InputSender::new();
        for code in 0..crate::input::HISTORY as u32 {
            tx.push(InputEvent::Key { code: 200 + code, pressed: true }, 0);
        }
        let p = tx.push(InputEvent::Scroll { dx: 1.0, dy: 1.0, v120_x: 120, v120_y: 120 }, 0);
        assert!(Datagram::Input(p).to_vec().len() < 1000);
    }

    #[test]
    fn garbage_is_dropped() {
        assert_eq!(Datagram::decode(&[]), None);
        assert_eq!(Datagram::decode(&[99, 1, 2]), None);
        assert_eq!(Datagram::decode(&[TAG_INPUT, 0xff]), None);
    }
}
