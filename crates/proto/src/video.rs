//! Video fragments (`docs/design.md` §2). Each encoded frame is split into
//! fragments that each fit one datagram. The header is fixed-size and
//! hand-encoded: it rides on every packet of the stream.

/// Set on every fragment of a frame the decoder can start from.
pub const FLAG_KEYFRAME: u8 = 1 << 0;

/// The header in front of each fragment's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    pub flags: u8,
    /// Video epoch: bumped on every encoder restart, such as a resize (§5).
    pub epoch: u16,
    /// Frame number, counting up across epochs.
    pub frame: u32,
    pub index: u16,
    pub count: u16,
    /// When the nested compositor committed the frame, in server µs.
    pub capture_us: u64,
    /// Commit to encoded, in µs.
    pub encode_us: u32,
}

impl FragmentHeader {
    pub const LEN: usize = 1 + 2 + 4 + 2 + 2 + 8 + 4;

    pub fn keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.encode_us.to_le_bytes());
    }

    /// Splits `buf` into the header and the fragment's payload.
    pub fn read(buf: &[u8]) -> Option<(Self, &[u8])> {
        if buf.len() < Self::LEN {
            return None;
        }
        let (h, payload) = buf.split_at(Self::LEN);
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let header = Self {
            flags: h[0],
            epoch: u16_at(1),
            frame: u32_at(3),
            index: u16_at(7),
            count: u16_at(9),
            capture_us: u64::from_le_bytes(h[11..19].try_into().unwrap()),
            encode_us: u32_at(19),
        };
        if header.count == 0 || header.index >= header.count {
            return None;
        }
        Some((header, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = FragmentHeader {
            flags: FLAG_KEYFRAME,
            epoch: 3,
            frame: 0xdead_beef,
            index: 7,
            count: 9,
            capture_us: 1 << 40,
            encode_us: 4321,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert_eq!(buf.len(), FragmentHeader::LEN);
        buf.extend_from_slice(b"payload");
        let (back, payload) = FragmentHeader::read(&buf).unwrap();
        assert_eq!(back, h);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn rejects_bad_index() {
        let h = FragmentHeader { flags: 0, epoch: 0, frame: 0, index: 2, count: 2, capture_us: 0, encode_us: 0 };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert!(FragmentHeader::read(&buf).is_none());
        assert!(FragmentHeader::read(&buf[..5]).is_none());
    }
}
