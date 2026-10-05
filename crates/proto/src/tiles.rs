//! Tiles (`docs/design.md` §2): the encoding when the server has no
//! hardware encoder. Each update is the damaged part of the screen, cut
//! into tiles that each decode on their own, packed into datagrams that
//! each hold whole tiles. The header is fixed-size and hand-encoded, like
//! video's; the tiles themselves are coded by `farsight-tiles`.

use serde::{Deserialize, Serialize};

/// A rectangle of the screen, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The smallest rectangle holding both.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let (x0, y0) = (self.x.min(other.x), self.y.min(other.y));
        let x1 = (self.x + self.w).max(other.x + other.w);
        let y1 = (self.y + self.h).max(other.y + other.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// The part inside `bounds`.
    pub fn clip(self, bounds: Rect) -> Rect {
        let x0 = self.x.max(bounds.x);
        let y0 = self.y.max(bounds.y);
        let x1 = (self.x + self.w).min(bounds.x + bounds.w);
        let y1 = (self.y + self.h).min(bounds.y + bounds.h);
        if x1 <= x0 || y1 <= y0 {
            return Rect::default();
        }
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// The grid tiles are cut on, in screen pixels, and the largest tile.
pub const CELL: u16 = 64;

/// Bytes in front of each tile's data: `x: u16, y: u16, w: u8, h: u8,
/// kind: u8, len: u16`.
pub const TILE_HEAD: usize = 9;

/// The rectangles of the tiles in a datagram body, as far as it is
/// well-formed.
pub fn tile_rects(mut body: &[u8]) -> impl Iterator<Item = Rect> + '_ {
    std::iter::from_fn(move || {
        if body.len() < TILE_HEAD {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
        let rect = Rect::new(u16_at(0), u16_at(2), body[4] as u16, body[5] as u16);
        let len = u16_at(7) as usize;
        body = body.get(TILE_HEAD + len..)?;
        Some(rect)
    })
}

/// The grid cells of `bounds` that `received` doesn't cover, merged into
/// runs along each row of cells. Tiles never cross a cell's edge, so a cell
/// is covered when its tiles' areas add up to its own.
pub fn missing(bounds: Rect, received: &[Rect]) -> Vec<Rect> {
    let mut area = std::collections::HashMap::<(u16, u16), u32>::new();
    for r in received {
        *area.entry((r.x / CELL, r.y / CELL)).or_default() += r.w as u32 * r.h as u32;
    }
    let mut out: Vec<Rect> = Vec::new();
    if bounds.is_empty() {
        return out;
    }
    for cy in bounds.y / CELL..(bounds.y + bounds.h).div_ceil(CELL) {
        for cx in bounds.x / CELL..(bounds.x + bounds.w).div_ceil(CELL) {
            let cell = Rect::new(cx * CELL, cy * CELL, CELL, CELL).clip(bounds);
            if area.get(&(cx, cy)).copied().unwrap_or(0) >= cell.w as u32 * cell.h as u32 {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.y == cell.y && last.h == cell.h && last.x + last.w == cell.x => last.w += cell.w,
                _ => out.push(cell),
            }
        }
    }
    out
}

/// The header in front of each tiles datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilesHeader {
    pub epoch: u16,
    /// Update number, counting up across epochs.
    pub update: u32,
    /// This datagram's place in the update, and how many it has.
    pub index: u16,
    pub count: u16,
    /// When the nested compositor committed the frame, in server µs.
    pub capture_us: u64,
    /// Commit to encoded, in µs.
    pub encode_us: u32,
    /// Everything the update covers. If any datagram of it is lost, the
    /// client asks for this again (`RequestRefresh`).
    pub bounds: Rect,
}

impl TilesHeader {
    pub const LEN: usize = 2 + 4 + 2 + 2 + 8 + 4 + 8;

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.update.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.extend_from_slice(&self.encode_us.to_le_bytes());
        for v in [self.bounds.x, self.bounds.y, self.bounds.w, self.bounds.h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    /// Splits `buf` into the header and the tiles.
    pub fn read(buf: &[u8]) -> Option<(Self, &[u8])> {
        if buf.len() < Self::LEN {
            return None;
        }
        let (h, body) = buf.split_at(Self::LEN);
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let header = Self {
            epoch: u16_at(0),
            update: u32_at(2),
            index: u16_at(6),
            count: u16_at(8),
            capture_us: u64::from_le_bytes(h[10..18].try_into().unwrap()),
            encode_us: u32_at(18),
            bounds: Rect::new(u16_at(22), u16_at(24), u16_at(26), u16_at(28)),
        };
        if header.count == 0 || header.index >= header.count {
            return None;
        }
        Some((header, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = TilesHeader {
            epoch: 2,
            update: 77,
            index: 1,
            count: 3,
            capture_us: 1 << 40,
            encode_us: 900,
            bounds: Rect::new(64, 128, 640, 64),
        };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert_eq!(buf.len(), TilesHeader::LEN);
        buf.push(9);
        assert_eq!(TilesHeader::read(&buf), Some((h, &[9u8][..])));
        assert!(TilesHeader::read(&buf[..5]).is_none());
    }

    #[test]
    fn missing_cells_are_found_and_merged() {
        let bounds = Rect::new(0, 0, 192, 100);
        // Row 0: the middle cell whole, the last in two halves. Row 1
        // (36 rows tall): nothing.
        let got = [Rect::new(64, 0, 64, 64), Rect::new(128, 0, 64, 32), Rect::new(128, 32, 64, 32)];
        assert_eq!(
            missing(bounds, &got),
            [Rect::new(0, 0, 64, 64), Rect::new(0, 64, 192, 36)]
        );
        let all: Vec<Rect> = (0..2u16)
            .flat_map(|cy| (0..3u16).map(move |cx| Rect::new(cx * 64, cy * 64, 64, 64).clip(bounds)))
            .collect();
        assert!(missing(bounds, &all).is_empty());
    }

    #[test]
    fn tile_rects_parse() {
        let mut body = Vec::new();
        for (x, len) in [(0u16, 2u16), (64, 0)] {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes());
            body.extend_from_slice(&[64, 32, 0]);
            body.extend_from_slice(&len.to_le_bytes());
            body.extend(std::iter::repeat_n(7, len as usize));
        }
        let rects: Vec<Rect> = tile_rects(&body).collect();
        assert_eq!(rects, [Rect::new(0, 0, 64, 32), Rect::new(64, 0, 64, 32)]);
        assert_eq!(tile_rects(&body[..10]).count(), 0);
    }

    #[test]
    fn rect_union_and_clip() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 5, 10, 10);
        assert_eq!(a.union(b), Rect::new(0, 0, 30, 15));
        assert_eq!(Rect::default().union(b), b);
        assert_eq!(b.clip(Rect::new(0, 0, 25, 25)), Rect::new(20, 5, 5, 10));
        assert!(a.clip(b).is_empty());
    }
}
