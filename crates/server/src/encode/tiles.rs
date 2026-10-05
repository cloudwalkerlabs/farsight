//! Tiles, when there is no hardware encoder (docs/design.md §2): the
//! damaged parts of the nested window are read back as RGBX on the main
//! thread and coded on the encode thread (`farsight-tiles`).

use farsight_proto::tiles::Rect;
use farsight_tiles::{CELL, Packer};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ExportMem;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};

/// Read backs per frame; more damage than this is read as one box.
const MAX_RECTS: usize = 8;

/// The main thread's half: the screen size.
pub struct Screen {
    pub width: i32,
    pub height: i32,
}

/// Damaged regions read back, each with its pixels (RGBX, rows `4 * w`).
pub struct Damage {
    pub regions: Vec<(Rect, Vec<u8>)>,
    pub options: farsight_tiles::Options,
}

/// `rects` grown to the tile grid, clipped to the screen and merged when
/// there are many.
pub fn align(rects: &[Rect], width: i32, height: i32) -> Vec<Rect> {
    let screen = Rect::new(0, 0, width as u16, height as u16);
    let mut out: Vec<Rect> = rects
        .iter()
        .map(|r| {
            let x0 = r.x / CELL * CELL;
            let y0 = r.y / CELL * CELL;
            let x1 = (r.x + r.w).div_ceil(CELL) * CELL;
            let y1 = (r.y + r.h).div_ceil(CELL) * CELL;
            Rect::new(x0, y0, x1 - x0, y1 - y0).clip(screen)
        })
        .filter(|r| !r.is_empty())
        .collect();
    // Overlaps would be sent twice: merge any that touch, until none do.
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..out.len() {
            for j in i + 1..out.len() {
                if !out[i].clip(out[j]).is_empty() {
                    out[i] = out[i].union(out[j]);
                    out.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
    if out.len() > MAX_RECTS {
        let all = out.iter().fold(Rect::default(), |a, &r| a.union(r));
        out = vec![all];
    }
    out
}

impl Screen {
    /// Reads `rects` (already aligned) back from `texture`. Waits for the
    /// GPU.
    pub fn read(
        &self,
        renderer: &mut GlesRenderer,
        texture: &GlesTexture,
        rects: &[Rect],
        options: farsight_tiles::Options,
    ) -> anyhow::Result<Damage> {
        let mut regions = Vec::with_capacity(rects.len());
        for &r in rects {
            let region = smithay::utils::Rectangle::new((r.x as i32, r.y as i32).into(), (r.w as i32, r.h as i32).into());
            let mapping = renderer.copy_texture(texture, region, Fourcc::Abgr8888)?;
            regions.push((r, renderer.map_texture(&mapping)?.to_vec()));
        }
        Ok(Damage { regions, options })
    }
}

/// The encode thread's half.
pub struct TileEncoder(farsight_tiles::Encoder);

/// One update's datagram bodies.
#[derive(Debug, Default)]
pub struct Update {
    pub bodies: Vec<Vec<u8>>,
    pub bounds: Rect,
    /// Tiles sent lossy, for refinement.
    pub lossy: Vec<Rect>,
}

impl TileEncoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(farsight_tiles::Encoder::new()?))
    }

    pub fn encode(&mut self, damage: Damage) -> anyhow::Result<Update> {
        let mut packer = Packer::default();
        let mut update = Update::default();
        for (rect, pixels) in &damage.regions {
            update.lossy.extend(self.0.encode(pixels, 4 * rect.w as usize, *rect, damage.options, &mut packer)?);
            update.bounds = update.bounds.union(*rect);
        }
        update.bodies = packer.bodies;
        Ok(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_is_aligned_clipped_and_merged() {
        let r = align(&[Rect::new(70, 10, 5, 5)], 1000, 500);
        assert_eq!(r, [Rect::new(64, 0, 64, 64)]);
        let r = align(&[Rect::new(990, 490, 10, 10)], 1000, 500);
        assert_eq!(r, [Rect::new(960, 448, 40, 52)]);
        // Two in one cell become one.
        let r = align(&[Rect::new(1, 1, 2, 2), Rect::new(10, 10, 2, 2)], 1000, 500);
        assert_eq!(r, [Rect::new(0, 0, 64, 64)]);
        // Far apart stay apart; many become one box.
        assert_eq!(align(&[Rect::new(0, 0, 1, 1), Rect::new(500, 300, 1, 1)], 1000, 500).len(), 2);
        let many: Vec<Rect> = (0..20).map(|i| Rect::new(i * 128, 0, 1, 1)).collect();
        assert_eq!(align(&many, 4000, 500), [Rect::new(0, 0, 2496, 64)]);
    }
}
