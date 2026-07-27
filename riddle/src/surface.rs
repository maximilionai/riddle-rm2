//! Drawing surface abstraction: same drawing code renders into either the
//! qtfb RGB565 shared memory (in-xochitl backend) or the vendor engine's
//! RGB32 aux framebuffer (takeover backend). Colors are RGB565 u16 at the
//! API; the surface converts on write.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    /// 2 bytes/px, little-endian RGB565 (qtfb FBFMT_RMPP_RGB565).
    Rgb565,
    /// 4 bytes/px, QImage Format_RGB32: bytes B,G,R,0xFF.
    Rgb32,
}

pub struct Surface {
    ptr: *mut u8,
    len: usize,
    pub w: usize,
    pub h: usize,
    pub stride: usize,
    pub fmt: PixFmt,
}

// Single-threaded writer over a long-lived mapping.
unsafe impl Send for Surface {}

pub const WHITE: u16 = 0xFFFF;
pub const BLACK: u16 = 0x0000;
/// Old ink: how the diary writes its memories (a readable e-ink gray).
pub const FADED: u16 = 0x7BCF;

#[inline]
fn expand565(c: u16) -> (u8, u8, u8) {
    let r = ((c >> 11) & 0x1f) as u32;
    let g = ((c >> 5) & 0x3f) as u32;
    let b = (c & 0x1f) as u32;
    (
        ((r * 255 + 15) / 31) as u8,
        ((g * 255 + 31) / 63) as u8,
        ((b * 255 + 15) / 31) as u8,
    )
}

impl Surface {
    pub fn new(ptr: *mut u8, len: usize, w: usize, h: usize, stride: usize, fmt: PixFmt) -> Self {
        Self {
            ptr,
            len,
            w,
            h,
            stride,
            fmt,
        }
    }

    #[inline]
    fn buf(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    #[inline]
    fn buf_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[inline]
    pub fn put_px(&mut self, x: i32, y: i32, c: u16) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let (stride, fmt) = (self.stride, self.fmt);
        match fmt {
            PixFmt::Rgb565 => {
                let i = y as usize * stride + x as usize * 2;
                let b = self.buf();
                b[i] = (c & 0xff) as u8;
                b[i + 1] = (c >> 8) as u8;
            }
            PixFmt::Rgb32 => {
                let (r, g, bl) = expand565(c);
                let i = y as usize * stride + x as usize * 4;
                let b = self.buf();
                b[i] = bl;
                b[i + 1] = g;
                b[i + 2] = r;
                b[i + 3] = 0xFF;
            }
        }
    }

    /// Luminance 0..255 — used by the PNG rasterizer and dissolve inkness test.
    #[inline]
    pub fn luma(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return 255;
        }
        let b = self.buf_ref();
        match self.fmt {
            PixFmt::Rgb565 => {
                let i = y as usize * self.stride + x as usize * 2;
                let px = (b[i] as u16) | ((b[i + 1] as u16) << 8);
                (((px >> 5) & 0x3f) as u32 * 255 / 63) as u8
            }
            PixFmt::Rgb32 => {
                let i = y as usize * self.stride + x as usize * 4;
                // Green approximates luma well enough for mono ink.
                b[i + 1]
            }
        }
    }

    /// Fill one row's `x0..x1` (x1 exclusive) with `c`. Both callers below are
    /// span-shaped, and going through [`Surface::put_px`] per pixel meant
    /// rebuilding the buffer slice from the raw pointer — and re-deciding the
    /// pixel format — once per pixel. A full-page `fill_rect` is 2.6M of those;
    /// `recompose` and `clear_page` do one on every undo and every erased mark.
    ///
    /// Coordinates must already be clipped to the surface.
    #[inline]
    fn fill_row(&mut self, y: usize, x0: usize, x1: usize, c: u16) {
        if x0 >= x1 {
            return;
        }
        let (stride, fmt, bpp) = (self.stride, self.fmt, self.bpp());
        let (s, e) = (y * stride + x0 * bpp, y * stride + x1 * bpp);
        let row = &mut self.buf()[s..e];
        match fmt {
            PixFmt::Rgb565 => {
                let p = c.to_le_bytes();
                // WHITE and BLACK are byte-uniform, so the hot cases become a
                // memset; FADED and friends fall back to a 2-byte stamp.
                if p[0] == p[1] {
                    row.fill(p[0]);
                } else {
                    for px in row.chunks_exact_mut(2) {
                        px.copy_from_slice(&p);
                    }
                }
            }
            PixFmt::Rgb32 => {
                let (r, g, b) = expand565(c);
                let p = [b, g, r, 0xFF];
                if p[0] == p[1] && p[1] == p[2] && p[2] == p[3] {
                    row.fill(p[0]);
                } else {
                    for px in row.chunks_exact_mut(4) {
                        px.copy_from_slice(&p);
                    }
                }
            }
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: u16) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        for row in y..y1 {
            self.fill_row(row, x.min(x1), x1, c);
        }
    }

    /// Invert the RGB of a rect (cursor/pressed-key feedback).
    pub fn invert_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        let (stride, fmt) = (self.stride, self.fmt);
        let buf = self.buf();
        for row in y..y1 {
            match fmt {
                PixFmt::Rgb565 => {
                    let s = row * stride + x * 2;
                    let e = row * stride + x1 * 2;
                    for b in &mut buf[s..e] {
                        *b = !*b;
                    }
                }
                PixFmt::Rgb32 => {
                    for col in x..x1 {
                        let i = row * stride + col * 4;
                        buf[i] = !buf[i];
                        buf[i + 1] = !buf[i + 1];
                        buf[i + 2] = !buf[i + 2];
                    }
                }
            }
        }
    }

    #[inline]
    fn bpp(&self) -> usize {
        match self.fmt {
            PixFmt::Rgb565 => 2,
            PixFmt::Rgb32 => 4,
        }
    }

    /// Snapshot a rect's raw bytes (for save-under panels).
    pub fn copy_rect(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        let bpp = self.bpp();
        let b = self.buf_ref();
        let mut out = Vec::with_capacity((x1 - x) * (y1 - y) * bpp);
        for row in y..y1 {
            let s = row * self.stride + x * bpp;
            out.extend_from_slice(&b[s..s + (x1 - x) * bpp]);
        }
        out
    }

    /// Put back bytes captured by `copy_rect` with the same geometry.
    pub fn paste_rect(&mut self, x: usize, y: usize, w: usize, h: usize, data: &[u8]) {
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        let (bpp, stride) = (self.bpp(), self.stride);
        let row_len = (x1 - x) * bpp;
        let b = self.buf();
        for (i, row) in (y..y1).enumerate() {
            let s = row * stride + x * bpp;
            b[s..s + row_len].copy_from_slice(&data[i * row_len..(i + 1) * row_len]);
        }
    }

    /// A filled disc of radius `r` centred on (`cx`, `cy`).
    ///
    /// One span per row rather than a bounds-checked write per pixel: this is
    /// the brush, so it runs for every point of every stroke the writer makes
    /// and every point the diary animates.
    pub fn stamp(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        if r < 0 {
            return;
        }
        let r2 = r * r;
        for dy in -r..=r {
            let y = cy + dy;
            if y < 0 || y >= self.h as i32 {
                continue;
            }
            // The old test was `dx*dx + dy*dy <= r*r`, i.e. |dx| <= sqrt(r²-dy²).
            // IEEE-754 sqrt is correctly rounded, so a perfect square comes back
            // exact and the truncation is a true floor — the disc is identical.
            let half = ((r2 - dy * dy) as f64).sqrt() as i32;
            let x0 = (cx - half).max(0);
            let x1 = (cx + half).min(self.w as i32 - 1);
            if x0 > x1 {
                continue;
            }
            self.fill_row(y as usize, x0 as usize, (x1 + 1) as usize, c);
        }
    }

    /// The pre-span implementation of [`Surface::stamp`], kept only so the
    /// tests can prove the fast path paints exactly the same disc.
    #[cfg(test)]
    fn stamp_per_pixel(&mut self, cx: i32, cy: i32, r: i32, c: u16) {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.put_px(cx + dx, cy + dy, c);
                }
            }
        }
    }

    pub fn brush_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: u16) {
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let steps = dx.max(dy).max(1);
        for i in 0..=steps {
            let x = x0 + (x1 - x0) * i / steps;
            let y = y0 + (y1 - y0) * i / steps;
            self.stamp(x, y, r, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;
    const H: usize = 48;

    fn surf(fmt: PixFmt) -> (Vec<u8>, Surface) {
        let bpp = match fmt {
            PixFmt::Rgb565 => 2,
            PixFmt::Rgb32 => 4,
        };
        let mut buf = vec![0u8; W * H * bpp];
        let ptr = buf.as_mut_ptr();
        let s = Surface::new(ptr, buf.len(), W, H, W * bpp, fmt);
        (buf, s)
    }

    fn pixels(s: &Surface) -> Vec<u8> {
        let mut out = Vec::with_capacity(W * H);
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                out.push(s.luma(x, y));
            }
        }
        out
    }

    /// The span-based brush must paint the same disc as the per-pixel one it
    /// replaced — including where it is clipped by every edge of the panel.
    #[test]
    fn span_stamp_matches_per_pixel_stamp() {
        for fmt in [PixFmt::Rgb565, PixFmt::Rgb32] {
            for r in 0..=12 {
                for &(cx, cy) in &[
                    (32, 24),  // clear of every edge
                    (0, 0),    // clipped in both axes
                    (63, 47),  // the far corner
                    (-3, 20),  // centre off the left edge
                    (20, -3),  // and off the top
                    (70, 24),  // wholly off the panel
                    (2, 45),
                ] {
                    let (_a, mut fast) = surf(fmt);
                    let (_b, mut slow) = surf(fmt);
                    fast.fill_rect(0, 0, W, H, WHITE);
                    slow.fill_rect(0, 0, W, H, WHITE);
                    fast.stamp(cx, cy, r, BLACK);
                    slow.stamp_per_pixel(cx, cy, r, BLACK);
                    assert_eq!(
                        pixels(&fast),
                        pixels(&slow),
                        "disc r={r} at ({cx},{cy}) differs"
                    );
                }
            }
        }
    }

    /// FADED is the one ink that is not byte-uniform in either format, so it
    /// exercises the 2- and 4-byte stamping path rather than the memset one.
    #[test]
    fn fill_rect_writes_the_colour_it_was_given() {
        for fmt in [PixFmt::Rgb565, PixFmt::Rgb32] {
            for c in [WHITE, BLACK, FADED] {
                let (_buf, mut s) = surf(fmt);
                s.fill_rect(0, 0, W, H, c);
                let want = s.luma(0, 0);
                for y in 0..H as i32 {
                    for x in 0..W as i32 {
                        assert_eq!(s.luma(x, y), want, "({x},{y}) missed the fill");
                    }
                }
            }
        }
    }

    #[test]
    fn fill_rect_clips_to_the_surface() {
        for fmt in [PixFmt::Rgb565, PixFmt::Rgb32] {
            let (_buf, mut s) = surf(fmt);
            s.fill_rect(0, 0, W, H, WHITE);
            // Overhangs the right and bottom edges; must not panic or wrap onto
            // the next row.
            s.fill_rect(W - 4, H - 4, 100, 100, BLACK);
            assert!(s.luma(W as i32 - 1, H as i32 - 1) < 50);
            assert!(s.luma(W as i32 - 5, H as i32 - 5) > 200);
            // Wholly outside: a no-op, not a wrap.
            s.fill_rect(W + 10, H + 10, 8, 8, BLACK);
            assert!(s.luma(0, 0) > 200);
        }
    }

    /// Ink shades must stay ordered after the format round trip, or the
    /// dissolve's `luma < 250` test and `is_blank`'s `luma < 200` stop agreeing
    /// with what is on the page.
    #[test]
    fn ink_shades_stay_ordered() {
        for fmt in [PixFmt::Rgb565, PixFmt::Rgb32] {
            let (_buf, mut s) = surf(fmt);
            s.fill_rect(0, 0, 8, 8, BLACK);
            s.fill_rect(8, 0, 8, 8, FADED);
            s.fill_rect(16, 0, 8, 8, WHITE);
            let (black, faded, white) = (s.luma(2, 2), s.luma(10, 2), s.luma(18, 2));
            assert!(black < faded && faded < white, "{black} {faded} {white}");
            assert!(black < 50 && white > 200 && (100..200).contains(&faded));
        }
    }
}
