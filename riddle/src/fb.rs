//! Geometry helpers. Drawing lives in surface.rs.
//!
//! reMarkable 2 port: the panel is 1404x1872 instead of the Paper Pro's
//! 1620x2160. Both are exactly 3:4, and 1404/1620 == 1872/2160 == 13/15, so
//! every hand-tuned layout constant in this app scales by ONE uniform factor
//! and no aspect-ratio rework is needed. `s()` / `sf()` / `su()` apply it.

pub const SCREEN_W: usize = 1404;
pub const SCREEN_H: usize = 1872;

/// The page these layout constants were authored against (reMarkable Paper Pro).
pub const REF_W: usize = 1620;

/// Scale a Paper-Pro-authored pixel constant to this panel.
#[inline]
pub const fn s(v: i32) -> i32 {
    v * SCREEN_W as i32 / REF_W as i32
}

/// Same, for unsigned layout math.
#[inline]
pub const fn su(v: usize) -> usize {
    v * SCREEN_W / REF_W
}

// For float measures (font sizes) the ratio is applied inline as
// `x * SCREEN_W as f32 / REF_W as f32`, which is const-evaluable.

/// Grow-only pixel bounding box, used to build update/dissolve regions.
#[derive(Clone, Copy, Debug)]
pub struct BBox {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl BBox {
    pub fn empty() -> Self {
        Self {
            x0: i32::MAX,
            y0: i32::MAX,
            x1: i32::MIN,
            y1: i32::MIN,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.x0 > self.x1
    }
    pub fn add(&mut self, x: i32, y: i32, margin: i32) {
        self.x0 = self.x0.min(x - margin).max(0);
        self.y0 = self.y0.min(y - margin).max(0);
        self.x1 = self.x1.max(x + margin).min(SCREEN_W as i32 - 1);
        self.y1 = self.y1.max(y + margin).min(SCREEN_H as i32 - 1);
    }
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        (
            self.x0,
            self.y0,
            self.x1 - self.x0 + 1,
            self.y1 - self.y0 + 1,
        )
    }
}
