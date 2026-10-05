//! Video fragments (`docs/design.md` §2). Each encoded frame is split into
//! `data` equal shards that each fit one datagram, the last padded with
//! zeros, followed by Reed-Solomon parity shards of the same size: any
//! `data` of a frame's `count` shards rebuild it. The header is fixed-size
//! and hand-encoded: it rides on every packet of the stream.
//!
//! The shard size is the payload's length. It may be more than the frame
//! needs: QUIC packs small datagrams into one packet, and shards that share
//! a packet are lost together, so a frame of more than one shard pads them
//! all past half a datagram.

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
    /// The newest frame this one may reference: the one before it,
    /// normally; an older one, after reference frame invalidation (RFI,
    /// §2); itself, for a keyframe. A client that decoded up to `refs`
    /// (from the last keyframe on) can decode it.
    pub refs: u32,
    /// Shards `0..data` are the frame's bytes, the rest parity.
    pub index: u16,
    /// Data and parity shards.
    pub count: u16,
    pub data: u16,
    /// The frame's length in bytes; what is past it in the data shards is
    /// padding.
    pub len: u32,
    /// When the nested compositor committed the frame, in server µs.
    pub capture_us: u64,
    /// Commit to encoded, in µs.
    pub encode_us: u32,
}

impl FragmentHeader {
    pub const LEN: usize = 1 + 2 + 4 + 4 + 2 + 2 + 2 + 4 + 8 + 4;

    pub fn keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.refs.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.data.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
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
            refs: u32_at(7),
            index: u16_at(11),
            count: u16_at(13),
            data: u16_at(15),
            len: u32_at(17),
            capture_us: u64::from_le_bytes(h[21..29].try_into().unwrap()),
            encode_us: u32_at(29),
        };
        if header.data == 0 || header.data > header.count || header.index >= header.count {
            return None;
        }
        // The data shards must hold the frame.
        if payload.len() < shard_size(header.len as usize, header.data as usize) || !payload.len().is_multiple_of(2) {
            return None;
        }
        Some((header, payload))
    }
}

/// Frame `a` comes before frame `b`, across wrap-around.
pub fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// The smallest shard size for a frame of `len` bytes in `data` shards:
/// as even as the split allows (Reed-Solomon works on pairs of bytes),
/// never empty.
pub fn shard_size(len: usize, data: usize) -> usize {
    (len.div_ceil(data.max(1)).max(1) + 1) & !1
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
            refs: 0xdead_beee,
            index: 7,
            count: 9,
            data: 8,
            len: 7 * 8 + 7,
            capture_us: 1 << 40,
            encode_us: 4321,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert_eq!(buf.len(), FragmentHeader::LEN);
        buf.extend_from_slice(b"payload!");
        let (back, payload) = FragmentHeader::read(&buf).unwrap();
        assert_eq!(back, h);
        assert_eq!(payload, b"payload!");
    }

    #[test]
    fn rejects_bad_shards() {
        let h = FragmentHeader {
            flags: 0,
            epoch: 0,
            frame: 0,
            refs: 0,
            index: 2,
            count: 2,
            data: 1,
            len: 4,
            capture_us: 0,
            encode_us: 0,
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert!(FragmentHeader::read(&buf).is_none());
        assert!(FragmentHeader::read(&buf[..5]).is_none());
        // Shards must hold the frame, in pairs of bytes.
        let mut buf = Vec::new();
        FragmentHeader { index: 1, ..h }.write(&mut buf);
        buf.extend_from_slice(&[0; 2]);
        assert!(FragmentHeader::read(&buf).is_none());
        buf.extend_from_slice(&[0; 3]);
        assert!(FragmentHeader::read(&buf).is_none());
        buf.push(0);
        assert!(FragmentHeader::read(&buf).is_some());
    }

    #[test]
    fn frame_order_wraps() {
        assert!(before(1, 2) && !before(2, 1) && !before(2, 2));
        assert!(before(u32::MAX, 0));
    }

    #[test]
    fn shard_sizes_are_even_and_cover_the_frame() {
        for (len, data) in [(0, 1), (1, 1), (1300, 2), (2401, 3), (1200, 1)] {
            let s = shard_size(len, data);
            assert!(s.is_multiple_of(2) && s * data >= len && s > 0, "{len} {data}");
        }
        assert_eq!(shard_size(1300, 2), 650);
    }
}
