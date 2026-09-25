//! The two `image/draw` paths the avatar goes through, and `golang.org/x/image/font.Drawer`.
//!
//! - `draw.Draw(dst, r, &image.Uniform{c}, image.Point{}, draw.Src)` onto an `*image.RGBA` is the
//!   `drawFillSrc` fast path (draw.go:411): every pixel of `r` set to `c`'s premultiplied 8-bit
//!   value.
//! - `draw.DrawMask(dst, dr, &image.Uniform{c}, image.Point{}, mask, mp, draw.Over)` with an
//!   `*image.Alpha` mask is `drawGlyphOver` (draw.go:617), after `clip` (draw.go:83).
//!
//! Only those two are ported: `image.Uniform`'s bounds are effectively infinite, so `clip` never
//! narrows by the source, and neither path reads the source point.

use goimage::image::{Pixels, Rect};

use crate::face::{AlphaMask, Face};
use crate::fixed::Point26_6;

/// `m = 1<<16 - 1` (draw.go:18).
const M: u32 = (1 << 16) - 1;

/// A colour as `color.Color.RGBA()` returns it: premultiplied, 16 bits per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba64 {
    pub r: u32,
    pub g: u32,
    pub b: u32,
    pub a: u32,
}

impl Rgba64 {
    /// `color.NRGBA{r, g, b, a}.RGBA()`: each channel premultiplied by `a`, widened to 16 bits.
    #[must_use]
    pub fn from_nrgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        let a16 = u32::from(a) | u32::from(a) << 8;
        let ch = |c: u8| {
            let c = u32::from(c) | u32::from(c) << 8;
            c * a16 / 0xffff
        };
        Rgba64 {
            r: ch(r),
            g: ch(g),
            b: ch(b),
            a: a16,
        }
    }

    /// `image.White` (`color.Gray16{0xffff}`).
    pub const WHITE: Rgba64 = Rgba64 {
        r: 0xffff,
        g: 0xffff,
        b: 0xffff,
        a: 0xffff,
    };
}

fn intersect(a: Rect, b: Rect) -> Rect {
    // `image.Rectangle.Intersect`: an empty result is the zero rectangle.
    let r = Rect {
        min_x: a.min_x.max(b.min_x),
        min_y: a.min_y.max(b.min_y),
        max_x: a.max_x.min(b.max_x),
        max_y: a.max_y.min(b.max_y),
    };
    if r.min_x >= r.max_x || r.min_y >= r.max_y {
        Rect::default()
    } else {
        r
    }
}

fn is_empty(r: &Rect) -> bool {
    r.min_x >= r.max_x || r.min_y >= r.max_y
}

/// `draw.Draw(dst, r, &image.Uniform{c}, image.Point{}, draw.Src)` for an `*image.RGBA` `dst`:
/// `clip`, then `drawFillSrc`.
pub fn fill_src(dst: &mut Pixels, r: Rect, c: Rgba64) {
    let r = intersect(r, dst.rect);
    if is_empty(&r) {
        return;
    }
    let (r8, g8, b8, a8) = (
        (c.r >> 8) as u8,
        (c.g >> 8) as u8,
        (c.b >> 8) as u8,
        (c.a >> 8) as u8,
    );
    for y in r.min_y..r.max_y {
        let i0 = dst.offset(r.min_x, y, 4);
        for px in dst.pix[i0..i0 + (r.dx() as usize) * 4].chunks_exact_mut(4) {
            px[0] = r8;
            px[1] = g8;
            px[2] = b8;
            px[3] = a8;
        }
    }
}

/// `draw.DrawMask(dst, dr, &image.Uniform{src}, image.Point{}, mask, mp, draw.Over)` for an
/// `*image.RGBA` `dst` and an `*image.Alpha` `mask` whose origin is (0, 0): `clip`, then
/// `drawGlyphOver`. The arithmetic is Go's `uint32`, wrapping included.
pub fn draw_glyph_over(dst: &mut Pixels, dr: Rect, src: Rgba64, mask: &AlphaMask, mp: (i64, i64)) {
    let orig = (dr.min_x, dr.min_y);
    let mut r = intersect(dr, dst.rect);
    let mask_rect = Rect {
        min_x: orig.0 - mp.0,
        min_y: orig.1 - mp.1,
        max_x: mask.width + orig.0 - mp.0,
        max_y: mask.height + orig.1 - mp.1,
    };
    r = intersect(r, mask_rect);
    if is_empty(&r) {
        return;
    }
    let mp = (mp.0 + (r.min_x - orig.0), mp.1 + (r.min_y - orig.1));

    let (sr, sg, sb, sa) = (src.r, src.g, src.b, src.a);
    let mut i0 = dst.offset(r.min_x, r.min_y, 4);
    let mut i1 = i0 + (r.dx() as usize) * 4;
    let mut mi0 = (mp.1 as usize) * mask.stride + mp.0 as usize;
    for _ in r.min_y..r.max_y {
        let (mut i, mut mi) = (i0, mi0);
        while i < i1 {
            let mut ma = u32::from(mask.pix[mi]);
            if ma != 0 {
                ma |= ma << 8;
                let a = (M - sa.wrapping_mul(ma) / M).wrapping_mul(0x101);
                let d = &mut dst.pix[i..i + 4];
                let blend = |dv: u8, sv: u32| -> u8 {
                    ((u32::from(dv)
                        .wrapping_mul(a)
                        .wrapping_add(sv.wrapping_mul(ma))
                        / M)
                        >> 8) as u8
                };
                d[0] = blend(d[0], sr);
                d[1] = blend(d[1], sg);
                d[2] = blend(d[2], sb);
                d[3] = blend(d[3], sa);
            }
            i += 4;
            mi += 1;
        }
        i0 += dst.stride;
        i1 += dst.stride;
        mi0 += mask.stride;
    }
}

/// `font.Drawer{Dst: dst, Src: &image.Uniform{src}, Face: face, Dot: dot}.DrawString(s)`: each
/// rune kerned against the one before it, its glyph drawn with [`draw_glyph_over`], and the dot
/// advanced. Returns the final dot. A rune whose glyph does not load draws nothing and advances
/// by zero, as Go's ignored `ok` gives.
pub fn draw_string(
    dst: &mut Pixels,
    src: Rgba64,
    face: &mut Face,
    mut dot: Point26_6,
    s: &str,
) -> Point26_6 {
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if let Some(p) = prev {
            dot.x = dot.x.wrapping_add(face.kern(p, c).unwrap_or(0));
        }
        if let Some(g) = face.glyph(dot, c) {
            let dr = Rect {
                min_x: g.dr.0,
                min_y: g.dr.1,
                max_x: g.dr.2,
                max_y: g.dr.3,
            };
            if !is_empty(&dr) {
                draw_glyph_over(dst, dr, src, &g.mask, (0, 0));
            }
            dot.x = dot.x.wrapping_add(g.advance);
        }
        prev = Some(c);
    }
    dot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opaque NRGBA widens each channel to 16 bits unchanged.
    #[test]
    fn an_opaque_nrgba_premultiplies_to_itself() {
        let c = Rgba64::from_nrgba(197, 8, 126, 255);
        assert_eq!(
            (c.r, c.g, c.b, c.a),
            (197 * 0x101, 8 * 0x101, 126 * 0x101, 0xffff)
        );
    }

    /// A full mask draws the source; a zero mask leaves the destination; a half mask blends.
    #[test]
    fn the_glyph_over_blend_is_gos() {
        let mut dst = Pixels::new(Rect::new(0, 0, 3, 1), 4);
        let whole = dst.rect;
        fill_src(&mut dst, whole, Rgba64::from_nrgba(100, 50, 20, 255));
        let mask = AlphaMask {
            pix: vec![255, 0, 128],
            stride: 3,
            width: 3,
            height: 1,
        };
        draw_glyph_over(
            &mut dst,
            Rect::new(0, 0, 3, 1),
            Rgba64::WHITE,
            &mask,
            (0, 0),
        );
        assert_eq!(&dst.pix[0..4], &[255, 255, 255, 255]);
        assert_eq!(&dst.pix[4..8], &[100, 50, 20, 255]);
        // 128 of 255 over (100,50,20), as Go's `image/draw` computes it (measured with a Go
        // program, not derived): 178, 153, 138.
        assert_eq!(&dst.pix[8..12], &[178, 153, 138, 255]);
    }
}
