//! Output layout. The client's window is the source of truth: it reports
//! its size in physical pixels and its scale, and the server applies both
//! to the output in one step (`docs/design.md` §5).

use serde::{Deserialize, Serialize};

/// Scale is carried in 1/120 steps, as in `wp_fractional_scale_v1`.
pub const SCALE_DENOMINATOR: u32 = 120;

/// One output, as the client wants it or as the server applied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// Width in physical pixels.
    pub width_px: u32,
    /// Height in physical pixels.
    pub height_px: u32,
    /// Scale in 1/120 steps: 120 is 1×, 180 is 1.5×.
    pub scale_120: u32,
    /// Refresh rate in millihertz.
    pub refresh_mhz: u32,
}

impl Layout {
    /// The size apps see, in logical pixels, rounded down.
    pub fn logical_size(&self) -> (u32, u32) {
        let s = self.scale_120.max(1);
        (
            self.width_px * SCALE_DENOMINATOR / s,
            self.height_px * SCALE_DENOMINATOR / s,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_size_applies_fractional_scale() {
        let layout = Layout {
            width_px: 2880,
            height_px: 1800,
            scale_120: 180,
            refresh_mhz: 60_000,
        };
        assert_eq!(layout.logical_size(), (1920, 1200));
    }
}
