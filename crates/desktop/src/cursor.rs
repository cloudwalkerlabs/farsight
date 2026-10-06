//! Cursor images at the right size. The server sends the nested
//! compositor's cursor at whatever density it was drawn (`scale_120`); the
//! window wants it in its own cursor pixels: physical pixels, which are the
//! remote output's, or points on macOS.

use farsight_proto::control::CursorImage;
use farsight_proto::layout::SCALE_DENOMINATOR;

/// `image` resampled to `density` image pixels per output pixel, with its
/// hotspot.
pub fn at_density(image: CursorImage, density: f64) -> CursorImage {
    let s = image.scale_120.max(1);
    let f = density * SCALE_DENOMINATOR as f64 / s as f64;
    if (f - 1.0).abs() < 1e-6 || image.width == 0 || image.height == 0 {
        return image;
    }
    let w = ((image.width as f64 * f).ceil() as u32).max(1);
    let h = ((image.height as f64 * f).ceil() as u32).max(1);
    let pixels = resample(&image.pixels, (image.width, image.height), (w, h));
    let hotspot = ((image.hotspot.0 as f64 * f) as i32, (image.hotspot.1 as f64 * f) as i32);
    let scale_120 = (density * SCALE_DENOMINATOR as f64).round() as u32;
    CursorImage { width: w, height: h, hotspot, scale_120, pixels, ..image }
}

/// Box-filtered resampling of premultiplied 4-byte pixels: each output
/// pixel averages the source pixels it covers (or the nearest one when
/// enlarging).
fn resample(src: &[u8], (sw, sh): (u32, u32), (dw, dh): (u32, u32)) -> Vec<u8> {
    let mut out = Vec::with_capacity((4 * dw * dh) as usize);
    let (fx, fy) = (sw as f64 / dw as f64, sh as f64 / dh as f64);
    for y in 0..dh {
        let (y0, y1) = span(y, fy, sh);
        for x in 0..dw {
            let (x0, x1) = span(x, fx, sw);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &src[4 * (sy * sw + sx) as usize..][..4];
                    for c in 0..4 {
                        acc[c] += p[c] as u32;
                    }
                }
            }
            let n = (x1 - x0) * (y1 - y0);
            out.extend(acc.map(|a| ((a + n / 2) / n) as u8));
        }
    }
    out
}

/// The source pixels output pixel `i` covers, at least one.
fn span(i: u32, f: f64, len: u32) -> (u32, u32) {
    let a = ((i as f64 * f) as u32).min(len - 1);
    let b = (((i + 1) as f64 * f).ceil() as u32).clamp(a + 1, len);
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_density_cursor_halves() {
        // 4×2 at 2×: two white pixels, two black, per row.
        let px: Vec<u8> = [255u8, 255, 0, 0].iter().flat_map(|&v| [v, v, v, 255]).cycle().take(32).collect();
        let img = CursorImage { id: 1, width: 4, height: 2, hotspot: (2, 2), scale_120: 240, pixels: px };
        let out = at_density(img, 1.0);
        assert_eq!((out.width, out.height, out.hotspot), (2, 1, (1, 1)));
        assert_eq!(out.pixels, [255, 255, 255, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn one_to_one_is_untouched() {
        let img = CursorImage { id: 1, width: 1, height: 1, hotspot: (0, 0), scale_120: 120, pixels: vec![1, 2, 3, 4] };
        assert_eq!(at_density(img.clone(), 1.0), img);
    }

    #[test]
    fn an_output_density_cursor_halves_into_points_at_2x() {
        // macOS at 2×: 48 output pixels are 24 points.
        let img = CursorImage { id: 1, width: 48, height: 48, hotspot: (8, 8), scale_120: 120, pixels: vec![0; 4 * 48 * 48] };
        let out = at_density(img, 0.5);
        assert_eq!((out.width, out.height, out.hotspot, out.scale_120), (24, 24, (4, 4), 60));
    }
}
