// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! The RGBA framebuffer and the 8x16 bitmap text. The menu is
//! drawn here, then encoded as one H.264 `I_PCM` picture (`h264`).

use crate::font;

/// `UiFb`: `w * h` pixels, 4 bytes each (R, G, B, A).
pub struct Fb<'a> {
    pub px: &'a mut [u8],
    pub w: i32,
    pub h: i32,
}

impl Fb<'_> {
    /// `put_px`: one pixel, clipped to the buffer.
    fn put(&mut self, x: i32, y: i32, rgb: (u8, u8, u8)) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        // Non-negative and inside w * h after the check above.
        let (Ok(x), Ok(y), Ok(w)) = (
            usize::try_from(x),
            usize::try_from(y),
            usize::try_from(self.w),
        ) else {
            return;
        };
        let at = (y * w + x) * 4;
        if let Some(p) = self.px.get_mut(at..at + 4) {
            p.copy_from_slice(&[rgb.0, rgb.1, rgb.2, 255]);
        }
    }

    /// `ui_fb_clear`: every pixel `rgb`, alpha 255.
    pub fn clear(&mut self, rgb: (u8, u8, u8)) {
        let n = usize::try_from(self.w.wrapping_mul(self.h)).unwrap_or(0);
        for p in self.px.as_chunks_mut::<4>().0.iter_mut().take(n) {
            *p = [rgb.0, rgb.1, rgb.2, 255];
        }
    }

    /// `ui_fb_rect`: a filled rectangle, clipped.
    pub fn rect(&mut self, x: i32, y: i32, rw: i32, rh: i32, rgb: (u8, u8, u8)) {
        for yy in y..y.wrapping_add(rh) {
            for xx in x..x.wrapping_add(rw) {
                self.put(xx, yy, rgb);
            }
        }
    }

    /// `ui_fb_text`: one line of text, top-left at (x, y), each font pixel
    /// `scale` x `scale`. A byte outside 32..126 draws as `?`.
    pub fn text(&mut self, x: i32, y: i32, s: &[u8], scale: i32, rgb: (u8, u8, u8)) {
        let mut cx = x;
        for &b in s {
            let c = if (font::FIRST..=font::LAST).contains(&b) {
                b
            } else {
                b'?'
            };
            let base = usize::from(c - font::FIRST) * 16;
            let glyph = &font::FONT[base..base + 16];
            for (row, &bits) in (0i32..).zip(glyph) {
                for col in 0..font::W {
                    if bits & (0x80u8 >> col) != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                self.put(cx + col * scale + sx, y + row * scale + sy, rgb);
                            }
                        }
                    }
                }
            }
            cx = cx.wrapping_add(font::W * scale);
        }
    }
}

/// `ui_fb_text_w`: the width of `s` in pixels.
#[must_use]
pub fn text_w(s: &[u8], scale: i32) -> i32 {
    i32::try_from(s.len())
        .unwrap_or(i32::MAX)
        .wrapping_mul(font::W)
        .wrapping_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_clips_and_text_draws() {
        let mut px = vec![0u8; 16 * 16 * 4];
        let mut fb = Fb {
            px: &mut px,
            w: 16,
            h: 16,
        };
        fb.clear((1, 2, 3));
        fb.rect(-4, -4, 6, 6, (9, 9, 9));
        fb.text(0, 0, b"\x01", 1, (7, 7, 7));
        assert_eq!(&px[0..4], &[9, 9, 9, 255]);
        assert_eq!(&px[2 * 4..2 * 4 + 4], &[1, 2, 3, 255]);
        assert_eq!(text_w(b"abc", 2), 48);
        // '?' has ink: some pixel in the 8x16 cell is 7.
        assert!(px.chunks(4).any(|p| p[0] == 7));
    }
}
