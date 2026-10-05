//! Tile coding (`docs/design.md` §2), as VNC's Tight does it: each tile of
//! the damage is sent as a fill, a palette with zlib, a JPEG, or (when it
//! must be lossless and has too many colours) zlib of its pixels.
//!
//! Tiles are packed whole into datagram bodies of at most a fixed size, so
//! every datagram decodes on its own; a tile too big for one is split into
//! quarters. Pixels are RGBX: R, G, B and a byte ignored, as the server's
//! GPU reads them back and as the client uploads them.
//!
//! A tile on the wire: `x: u16, y: u16, w: u8, h: u8, kind: u8, len: u16`,
//! then `len` bytes of data.

mod jpeg;

use std::io::{Read, Write};

use anyhow::{Context, bail, ensure};
use farsight_proto::tiles::{Rect, TILE_HEAD};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

pub use farsight_proto::tiles::CELL;

/// Tiles are split no smaller than this.
const MIN_TILE: u16 = 8;

/// The largest datagram body: whole tiles up to this many bytes. Small
/// enough for any path QUIC runs on (its datagrams hold at least about
/// 1150 bytes after headers).
pub const MAX_BODY: usize = 1100;

/// Palettes of up to this many colours are lossless.
const MAX_PALETTE: usize = 256;

const KIND_FILL: u8 = 0;
const KIND_PALETTE: u8 = 1;
const KIND_JPEG: u8 = 2;
const KIND_ZLIB: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// JPEG quality, 1–100.
    pub quality: u8,
    /// JPEG without chroma subsampling, for sharp coloured text.
    pub chroma444: bool,
    /// No JPEG: tiles with too many colours for a palette go as zlib.
    pub lossless: bool,
}

/// Collects tiles into datagram bodies.
#[derive(Debug, Default)]
pub struct Packer {
    pub bodies: Vec<Vec<u8>>,
}

impl Packer {
    fn push(&mut self, rect: Rect, kind: u8, data: &[u8]) {
        let len = TILE_HEAD + data.len();
        if self.bodies.last().is_none_or(|b| b.len() + len > MAX_BODY) {
            self.bodies.push(Vec::with_capacity(MAX_BODY));
        }
        let b = self.bodies.last_mut().unwrap();
        b.extend_from_slice(&rect.x.to_le_bytes());
        b.extend_from_slice(&rect.y.to_le_bytes());
        b.push(rect.w as u8);
        b.push(rect.h as u8);
        b.push(kind);
        b.extend_from_slice(&(data.len() as u16).to_le_bytes());
        b.extend_from_slice(data);
    }
}

/// The colours of a tile, if few enough for a palette.
struct Palette {
    colours: Vec<u32>,
    /// Open addressing: colour + 1 (0 is empty) and its index.
    table: Vec<(u32, u8)>,
}

impl Palette {
    fn new() -> Self {
        Self { colours: Vec::with_capacity(MAX_PALETTE), table: vec![(0, 0); 1024] }
    }

    fn clear(&mut self) {
        self.colours.clear();
        self.table.fill((0, 0));
    }

    /// The colour's index, adding it if new; `None` once there are too
    /// many.
    fn index(&mut self, c: u32) -> Option<u8> {
        let key = c + 1;
        let mut i = (c.wrapping_mul(0x9E37_79B1) >> 22) as usize;
        loop {
            match self.table[i] {
                (0, _) => {
                    if self.colours.len() == MAX_PALETTE {
                        return None;
                    }
                    let idx = self.colours.len() as u8;
                    self.colours.push(c);
                    self.table[i] = (key, idx);
                    return Some(idx);
                }
                (k, idx) if k == key => return Some(idx),
                _ => i = (i + 1) & 1023,
            }
        }
    }
}

fn rgb(px: &[u8]) -> u32 {
    px[0] as u32 | (px[1] as u32) << 8 | (px[2] as u32) << 16
}

fn bits_for(colours: usize) -> usize {
    match colours {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

pub struct Encoder {
    jpeg: jpeg::Compressor,
    palette: Palette,
    indices: Vec<u8>,
    data: Vec<u8>,
}

impl Encoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { jpeg: jpeg::Compressor::new()?, palette: Palette::new(), indices: Vec::new(), data: Vec::new() })
    }

    /// Encodes `rect` of the screen. `pixels` holds it, RGBX with rows
    /// `stride` bytes apart, starting at its top-left corner. Tiles follow
    /// the [`CELL`] grid relative to `rect`. Returns the tiles sent lossy,
    /// for refinement later.
    pub fn encode(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rect: Rect,
        opts: Options,
        packer: &mut Packer,
    ) -> anyhow::Result<Vec<Rect>> {
        ensure!(rect.h == 0 || pixels.len() >= (rect.h as usize - 1) * stride + 4 * rect.w as usize, "pixels too short");
        let mut lossy = Vec::new();
        for ty in (0..rect.h).step_by(CELL as usize) {
            for tx in (0..rect.w).step_by(CELL as usize) {
                let tile = Rect::new(tx, ty, CELL.min(rect.w - tx), CELL.min(rect.h - ty));
                self.tile(pixels, stride, rect, tile, opts, packer, &mut lossy)?;
            }
        }
        Ok(lossy)
    }

    /// `tile` is relative to `rect`.
    #[allow(clippy::too_many_arguments)]
    fn tile(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rect: Rect,
        tile: Rect,
        opts: Options,
        packer: &mut Packer,
        lossy: &mut Vec<Rect>,
    ) -> anyhow::Result<()> {
        let at = tile.y as usize * stride + tile.x as usize * 4;
        let px = &pixels[at..];
        let (w, h) = (tile.w as usize, tile.h as usize);
        let kind = self.code(px, stride, w, h, opts)?;
        let screen = Rect::new(rect.x + tile.x, rect.y + tile.y, tile.w, tile.h);
        if TILE_HEAD + self.data.len() > MAX_BODY {
            ensure!(tile.w > MIN_TILE || tile.h > MIN_TILE, "an {w}x{h} tile does not fit a datagram");
            let (hw, hh) = (tile.w.div_ceil(2), tile.h.div_ceil(2));
            for (dx, dy, qw, qh) in [(0, 0, hw, hh), (hw, 0, tile.w - hw, hh), (0, hh, hw, tile.h - hh), (hw, hh, tile.w - hw, tile.h - hh)] {
                if qw > 0 && qh > 0 {
                    self.tile(pixels, stride, rect, Rect::new(tile.x + dx, tile.y + dy, qw, qh), opts, packer, lossy)?;
                }
            }
            return Ok(());
        }
        if kind == KIND_JPEG {
            lossy.push(screen);
        }
        packer.push(screen, kind, &self.data);
        Ok(())
    }

    /// Codes one tile into `self.data`; returns its kind.
    fn code(&mut self, px: &[u8], stride: usize, w: usize, h: usize, opts: Options) -> anyhow::Result<u8> {
        self.data.clear();
        self.palette.clear();
        self.indices.clear();
        let mut few = true;
        'rows: for y in 0..h {
            let row = &px[y * stride..][..4 * w];
            let mut last = (u32::MAX, 0);
            for p in row.as_chunks::<4>().0 {
                let c = rgb(p);
                if c != last.0 {
                    match self.palette.index(c) {
                        Some(i) => last = (c, i),
                        None => {
                            few = false;
                            break 'rows;
                        }
                    }
                }
                self.indices.push(last.1);
            }
        }
        if few && self.palette.colours.len() == 1 {
            self.data.extend_from_slice(&self.palette.colours[0].to_le_bytes()[..3]);
            return Ok(KIND_FILL);
        }
        if few {
            let n = self.palette.colours.len();
            self.data.push((n - 1) as u8);
            for c in &self.palette.colours {
                self.data.extend_from_slice(&c.to_le_bytes()[..3]);
            }
            let bits = bits_for(n);
            let per_byte = 8 / bits;
            let mut packed = Vec::with_capacity(h * w.div_ceil(per_byte));
            for row in self.indices.chunks_exact(w) {
                for group in row.chunks(per_byte) {
                    let mut b = 0u8;
                    for (i, &idx) in group.iter().enumerate() {
                        b |= idx << (8 - bits * (i + 1));
                    }
                    packed.push(b);
                }
            }
            zlib(&packed, &mut self.data)?;
            return Ok(KIND_PALETTE);
        }
        if opts.lossless {
            let mut raw = Vec::with_capacity(3 * w * h);
            for y in 0..h {
                for p in px[y * stride..][..4 * w].as_chunks::<4>().0 {
                    raw.extend_from_slice(&p[..3]);
                }
            }
            zlib(&raw, &mut self.data)?;
            return Ok(KIND_ZLIB);
        }
        let mut jpeg = Vec::new();
        self.jpeg.compress(px, stride, (w, h), opts.quality, opts.chroma444, &mut jpeg)?;
        strip_tables(&jpeg, &mut self.data);
        Ok(KIND_JPEG)
    }
}

fn zlib(data: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    let mut z = ZlibEncoder::new(out, Compression::fast());
    z.write_all(data)?;
    z.finish()?;
    Ok(())
}

/// Drops the JFIF header (APP0) and the Huffman tables (DHT) from a
/// baseline JPEG: about 440 bytes that are the same in every tile.
/// libjpeg-turbo falls back to the standard tables when a JPEG has none,
/// as Motion JPEG relies on, and TurboJPEG writes exactly those.
fn strip_tables(jpeg: &[u8], out: &mut Vec<u8>) {
    let mut i = 2;
    out.extend_from_slice(&jpeg[..2]); // SOI
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let marker = jpeg[i + 1];
        if marker == 0xDA {
            break; // start of scan: the rest is entropy-coded data
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if marker != 0xE0 && marker != 0xC4 {
            out.extend_from_slice(&jpeg[i..i + 2 + len]);
        }
        i += 2 + len;
    }
    out.extend_from_slice(&jpeg[i..]);
}

pub struct Decoder {
    jpeg: jpeg::Decompressor,
    rgbx: Vec<u8>,
    inflated: Vec<u8>,
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { jpeg: jpeg::Decompressor::new()?, rgbx: Vec::new(), inflated: Vec::new() })
    }

    /// Decodes every tile in a datagram body, handing each to `f` as its
    /// screen rectangle and its RGBX pixels (rows `4 * w` bytes).
    pub fn decode(&mut self, mut body: &[u8], mut f: impl FnMut(Rect, &[u8])) -> anyhow::Result<()> {
        while !body.is_empty() {
            ensure!(body.len() >= TILE_HEAD, "truncated tile header");
            let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
            let rect = Rect::new(u16_at(0), u16_at(2), body[4] as u16, body[5] as u16);
            let kind = body[6];
            let len = u16_at(7) as usize;
            ensure!(body.len() >= TILE_HEAD + len, "truncated tile");
            ensure!(!rect.is_empty(), "empty tile");
            let data = &body[TILE_HEAD..TILE_HEAD + len];
            body = &body[TILE_HEAD + len..];
            self.tile(rect, kind, data).with_context(|| format!("tile {rect:?}"))?;
            f(rect, &self.rgbx);
        }
        Ok(())
    }

    fn tile(&mut self, rect: Rect, kind: u8, data: &[u8]) -> anyhow::Result<()> {
        let (w, h) = (rect.w as usize, rect.h as usize);
        self.rgbx.clear();
        match kind {
            KIND_FILL => {
                ensure!(data.len() == 3, "bad fill");
                for _ in 0..w * h {
                    self.rgbx.extend_from_slice(&[data[0], data[1], data[2], 255]);
                }
            }
            KIND_PALETTE => {
                ensure!(!data.is_empty(), "empty palette");
                let n = data[0] as usize + 1;
                ensure!(data.len() > 3 * n, "short palette");
                let colours = &data[1..1 + 3 * n];
                let bits = bits_for(n);
                let per_byte = 8 / bits;
                let row_bytes = w.div_ceil(per_byte);
                self.inflated.clear();
                ZlibDecoder::new(&data[1 + 3 * n..]).read_to_end(&mut self.inflated)?;
                ensure!(self.inflated.len() == row_bytes * h, "palette indices are the wrong size");
                let mask = ((1u16 << bits) - 1) as u8;
                for row in self.inflated.chunks_exact(row_bytes) {
                    for x in 0..w {
                        let idx = (row[x / per_byte] >> (8 - bits * (x % per_byte + 1))) & mask;
                        let c = colours.get(3 * idx as usize..3 * idx as usize + 3).context("index outside the palette")?;
                        self.rgbx.extend_from_slice(&[c[0], c[1], c[2], 255]);
                    }
                }
            }
            KIND_ZLIB => {
                self.inflated.clear();
                ZlibDecoder::new(data).read_to_end(&mut self.inflated)?;
                ensure!(self.inflated.len() == 3 * w * h, "zlib tile is the wrong size");
                for p in self.inflated.as_chunks::<3>().0 {
                    self.rgbx.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
            }
            KIND_JPEG => self.jpeg.decompress(data, (w, h), &mut self.rgbx)?,
            other => bail!("unknown tile kind {other}"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: Options = Options { quality: 85, chroma444: true, lossless: false };

    /// A `w`×`h` RGBX picture from `f(x, y) -> rgb`, with a stride wider
    /// than its rows.
    fn picture(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> (Vec<u8>, usize) {
        let stride = 4 * w + 12;
        let mut px = vec![0; stride * h];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                px[y * stride + 4 * x..][..4].copy_from_slice(&[r, g, b, 0]);
            }
        }
        (px, stride)
    }

    /// Decodes every body onto a canvas of `w`×`h`; returns it as RGB.
    fn render(bodies: &[Vec<u8>], w: usize, h: usize) -> Vec<[u8; 3]> {
        let mut canvas = vec![[0u8; 3]; w * h];
        let mut d = Decoder::new().unwrap();
        for b in bodies {
            assert!(b.len() <= MAX_BODY);
            d.decode(b, |r, px| {
                for y in 0..r.h as usize {
                    for x in 0..r.w as usize {
                        let p = &px[4 * (y * r.w as usize + x)..];
                        canvas[(r.y as usize + y) * w + r.x as usize + x] = [p[0], p[1], p[2]];
                    }
                }
            })
            .unwrap();
        }
        canvas
    }

    fn encode(px: &[u8], stride: usize, rect: Rect, opts: Options) -> (Packer, Vec<Rect>) {
        let mut packer = Packer::default();
        let lossy = Encoder::new().unwrap().encode(px, stride, rect, opts, &mut packer).unwrap();
        (packer, lossy)
    }

    #[test]
    fn flat_and_text_like_tiles_are_lossless() {
        // A flat background with "text": a few colours, odd size.
        let (w, h) = (150, 70);
        let f = |x: usize, y: usize| match (x / 3 + y / 5) % 7 {
            0 => [0, 0, 0],
            1 => [40, 40, 40],
            2 => [200, 30, 30],
            _ => [250, 250, 250],
        };
        let (px, stride) = picture(w, h, f);
        let (packer, lossy) = encode(&px, stride, Rect::new(0, 0, w as u16, h as u16), OPTS);
        assert!(lossy.is_empty());
        let canvas = render(&packer.bodies, w, h);
        for y in 0..h {
            for x in 0..w {
                assert_eq!(canvas[y * w + x], f(x, y), "at {x},{y}");
            }
        }
        // A blank screen is a few fills.
        let (px, stride) = picture(1920, 1080, |_, _| [10, 20, 30]);
        let (packer, _) = encode(&px, stride, Rect::new(0, 0, 1920, 1080), OPTS);
        let bytes: usize = packer.bodies.iter().map(Vec::len).sum();
        assert!(bytes < 30 * 17 * (TILE_HEAD + 3) + 1, "{bytes} bytes");
    }

    #[test]
    fn photos_go_as_jpeg_and_land_close() {
        let (w, h) = (100, 90);
        let f = |x: usize, y: usize| [(x * 2) as u8, (y * 2) as u8, (x + y) as u8];
        let (px, stride) = picture(w, h, f);
        let rect = Rect::new(320, 64, w as u16, h as u16);
        let (packer, lossy) = encode(&px, stride, rect, OPTS);
        assert!(!lossy.is_empty());
        assert!(lossy.iter().all(|r| r.x >= 320 && r.y >= 64));
        // Draw at the rect's place on a larger canvas.
        let canvas = render(&packer.bodies, 320 + w, 64 + h);
        let mut worst = 0;
        for y in 0..h {
            for x in 0..w {
                let got = canvas[(64 + y) * (320 + w) + 320 + x];
                let want = f(x, y);
                for c in 0..3 {
                    worst = worst.max((got[c] as i32 - want[c] as i32).abs());
                }
            }
        }
        assert!(worst < 40, "worst error {worst}");
        // Lossless instead: exact.
        let (packer, lossy) = encode(&px, stride, rect, Options { lossless: true, ..OPTS });
        assert!(lossy.is_empty());
        let canvas = render(&packer.bodies, 320 + w, 64 + h);
        assert_eq!(canvas[(64 + 5) * (320 + w) + 320 + 7], f(7, 5));
    }

    #[test]
    fn stripped_jpeg_is_small() {
        let (px, stride) = picture(16, 16, |x, y| [(x * 16) as u8, (y * 16) as u8, 128]);
        let mut full = Vec::new();
        jpeg::Compressor::new().unwrap().compress(&px, stride, (16, 16), 85, true, &mut full).unwrap();
        let mut stripped = Vec::new();
        strip_tables(&full, &mut stripped);
        assert!(full.len() - stripped.len() > 400, "{} -> {}", full.len(), stripped.len());
    }

    #[test]
    fn garbage_is_an_error() {
        let mut d = Decoder::new().unwrap();
        assert!(d.decode(&[1, 2, 3], |_, _| {}).is_err());
        assert!(d.decode(&[0, 0, 0, 0, 4, 4, KIND_JPEG, 3, 0, 1, 2, 3], |_, _| {}).is_err());
    }
}
