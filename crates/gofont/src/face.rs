//! Port of `github.com/golang/freetype/truetype/face.go`: `NewFace` and the `font.Face` methods
//! the avatar's `font.Drawer` calls — `GlyphBounds`, `GlyphAdvance`, `Glyph`, `Kern`, `Metrics`.
//!
//! # The glyph cache is not reproduced, and does not need to be
//!
//! Go's face keeps a 512-entry glyph cache backed by one tall `image.Alpha` of `maxw × maxh·512`
//! pixels, and `Glyph` returns that whole image with a `maskp` pointing at the entry's rows. What
//! reaches a destination is only the entry's `maxw × maxh` window, painted after it is cleared,
//! with spans clipped to that window. So this face rasterises into one `maxw × maxh` buffer per
//! call and returns it with `maskp = (0, 0)`: the pixels `DrawMask` reads are the same bytes, and
//! the cache — a speed-up for drawing the same glyph twice — is gone.
//!
//! # Sub-pixel positioning is Go's default
//!
//! `SubPixelsX` defaults to 4 (quarter-pixel dots) and `SubPixelsY` to 1. Note Go's `subPixelsY`
//! reads `o.SubPixelsX` — a bug in the original, reproduced: a caller who sets `SubPixelsX` gets
//! the same quantum vertically.

use crate::fixed::{Int26_6, Point26_6, Rectangle26_6};
use crate::glyph::{GlyphBuf, Point, on_curve};
use crate::raster::{Painter, Rasterizer, Span};
use crate::truetype::{Font, FontError, Index};

/// `truetype.Options`, less `GlyphCacheEntries` (see the module note).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Options {
    /// `Size` in points; 12 when not positive.
    pub size: f64,
    /// `DPI`; 72 when not positive.
    pub dpi: f64,
    /// `Hinting`: only `false` (Go's `HintingNone`) is supported.
    pub hinting: bool,
    /// `SubPixelsX`; 4 unless 1, 2, 4, …, 64.
    pub sub_pixels_x: i64,
    /// `SubPixelsY` — ignored, as Go ignores it (see the module note).
    pub sub_pixels_y: i64,
}

impl Options {
    fn size(&self) -> f64 {
        if self.size > 0.0 { self.size } else { 12.0 }
    }

    fn dpi(&self) -> f64 {
        if self.dpi > 0.0 { self.dpi } else { 72.0 }
    }

    /// `subPixels(q)` for a `q` already checked to be a power of two in 1..=64.
    fn sub_pixels(q: i32) -> (u32, Int26_6, Int26_6) {
        (q as u32, 32 / q, -64 / q)
    }

    /// A valid quantum, or `None` — Go's `case 1, 2, 4, 8, 16, 32, 64`.
    fn quantum(q: i64) -> Option<i32> {
        match q {
            1 | 2 | 4 | 8 | 16 | 32 | 64 => Some(q as i32),
            _ => None,
        }
    }

    fn sub_pixels_x(&self) -> (u32, Int26_6, Int26_6) {
        Self::sub_pixels(Self::quantum(self.sub_pixels_x).unwrap_or(4))
    }

    fn sub_pixels_y(&self) -> (u32, Int26_6, Int26_6) {
        // `o.SubPixelsX`, as in Go.
        Self::sub_pixels(Self::quantum(self.sub_pixels_x).unwrap_or(1))
    }
}

/// `font.Metrics`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub height: Int26_6,
    pub ascent: Int26_6,
    pub descent: Int26_6,
}

/// An 8-bit alpha mask, `image.Alpha`: `pix[y * stride + x]`, origin at (0, 0).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlphaMask {
    pub pix: Vec<u8>,
    pub stride: usize,
    pub width: i64,
    pub height: i64,
}

/// What `Face::glyph` returns: Go's `(dr, mask, maskp, advance)`, with the mask's origin at
/// `maskp = (0, 0)` (see the module note).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Glyph {
    /// `dr`: where the glyph lands in the destination, `(min_x, min_y, max_x, max_y)`.
    pub dr: (i64, i64, i64, i64),
    pub mask: AlphaMask,
    pub advance: Int26_6,
}

/// `glyphCacheVal` plus the mask it describes: what `face.rasterize` produces.
struct Rasterized {
    advance: Int26_6,
    offset: (i64, i64),
    gw: i64,
    gh: i64,
    mask: AlphaMask,
}

/// `truetype.face`.
#[derive(Clone, Debug)]
pub struct Face {
    font: Font,
    scale: Int26_6,
    sub_pixel_x: u32,
    sub_pixel_bias_x: Int26_6,
    sub_pixel_mask_x: Int26_6,
    sub_pixel_y: u32,
    sub_pixel_bias_y: Int26_6,
    sub_pixel_mask_y: Int26_6,
    maxw: i64,
    maxh: i64,
    r: Rasterizer,
    glyph_buf: GlyphBuf,
}

/// The mask painter: `facePainter.Paint`, clipped to the one entry's window.
struct MaskPainter<'a> {
    mask: &'a mut AlphaMask,
}

impl Painter for MaskPainter<'_> {
    fn paint(&mut self, spans: &[Span], _done: bool) {
        let m = &mut *self.mask;
        for s in spans {
            // Go offsets `s.Y` by the entry's first row and clips to `[paintOffset,
            // paintOffset+maxh)`; with the window at the origin that is `[0, maxh)`, and a span
            // at or past the last row ends the batch (`return`, not `continue`).
            if s.y < 0 {
                continue;
            }
            if s.y >= m.height {
                return;
            }
            let x0 = s.x0.max(0);
            let x1 = s.x1.min(m.width);
            if x0 >= x1 {
                continue;
            }
            let base = (s.y as usize) * m.stride;
            let color = (s.alpha >> 8) as u8;
            for p in &mut m.pix[base + x0 as usize..base + x1 as usize] {
                *p = color;
            }
        }
    }
}

impl Face {
    /// Port of `truetype.NewFace`. Hinting is refused rather than ignored; see [`crate::glyph`].
    pub fn new(font: Font, opts: &Options) -> Result<Face, FontError> {
        if opts.hinting {
            return Err(FontError::Unsupported("hinting".to_owned()));
        }
        // `fixed.Int26_6(0.5 + (size * dpi * 64 / 72))`: Go's float-to-int32 conversion truncates.
        let scale = (0.5 + (opts.size() * opts.dpi() * 64.0 / 72.0)) as Int26_6;
        let (sub_pixel_x, sub_pixel_bias_x, sub_pixel_mask_x) = opts.sub_pixels_x();
        let (sub_pixel_y, sub_pixel_bias_y, sub_pixel_mask_y) = opts.sub_pixels_y();
        let b = font.bounds(scale);
        let xmin = i64::from(b.min.x) >> 6;
        let ymin = -(i64::from(b.max.y)) >> 6;
        let xmax = (i64::from(b.max.x) + 63) >> 6;
        let ymax = -(i64::from(b.min.y) - 63) >> 6;
        let maxw = xmax - xmin;
        let maxh = ymax - ymin;
        let mut r = Rasterizer::default();
        r.set_bounds(maxw, maxh);
        Ok(Face {
            font,
            scale,
            sub_pixel_x,
            sub_pixel_bias_x,
            sub_pixel_mask_x,
            sub_pixel_y,
            sub_pixel_bias_y,
            sub_pixel_mask_y,
            maxw,
            maxh,
            r,
            glyph_buf: GlyphBuf::default(),
        })
    }

    /// The font this face draws.
    #[must_use]
    pub fn font(&self) -> &Font {
        &self.font
    }

    /// `face.scale`: the size in 26.6 pixels per em.
    #[must_use]
    pub fn scale(&self) -> Int26_6 {
        self.scale
    }

    /// Port of `face.Metrics`.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        let scale = f64::from(self.scale);
        let fupe = f64::from(self.font.units_per_em());
        Metrics {
            height: self.scale,
            ascent: (scale * f64::from(self.font.ascent()) / fupe).ceil() as Int26_6,
            descent: (scale * f64::from(-self.font.descent()) / fupe).ceil() as Int26_6,
        }
    }

    /// Port of `face.Kern` (unhinted: no rounding).
    pub fn kern(&self, r0: char, r1: char) -> Result<Int26_6, FontError> {
        self.font
            .kern(self.scale, self.font.index(r0), self.font.index(r1))
    }

    /// Port of `face.GlyphBounds`: the glyph's bounds (y down) and advance, or `None` for a glyph
    /// that does not load or whose bounds are inverted.
    pub fn glyph_bounds(&mut self, r: char) -> Option<(Rectangle26_6, Int26_6)> {
        let index = self.font.index(r);
        self.glyph_buf.load(&self.font, self.scale, index).ok()?;
        let b = self.glyph_buf.bounds;
        let (xmin, ymin, xmax, ymax) = (
            b.min.x,
            b.max.y.wrapping_neg(),
            b.max.x,
            b.min.y.wrapping_neg(),
        );
        if xmin > xmax || ymin > ymax {
            return None;
        }
        Some((
            Rectangle26_6 {
                min: Point26_6::new(xmin, ymin),
                max: Point26_6::new(xmax, ymax),
            },
            self.glyph_buf.advance_width,
        ))
    }

    /// Port of `face.GlyphAdvance`.
    pub fn glyph_advance(&mut self, r: char) -> Option<Int26_6> {
        let index = self.font.index(r);
        self.glyph_buf.load(&self.font, self.scale, index).ok()?;
        Some(self.glyph_buf.advance_width)
    }

    /// Port of `face.Glyph`: the glyph for `r` drawn at `dot`, quantised to the sub-pixel grid.
    pub fn glyph(&mut self, dot: Point26_6, r: char) -> Option<Glyph> {
        let dot_x = dot.x.wrapping_add(self.sub_pixel_bias_x) & self.sub_pixel_mask_x;
        let dot_y = dot.y.wrapping_add(self.sub_pixel_bias_y) & self.sub_pixel_mask_y;
        let (ix, fx) = (i64::from(dot_x >> 6), dot_x & 0x3f);
        let (iy, fy) = (i64::from(dot_y >> 6), dot_y & 0x3f);
        let index = self.font.index(r);
        // Go computes the cache slot from `sub_pixel_x`/`sub_pixel_y` here; the slot only picks
        // which rows of the tall mask are used, which the module note explains away.
        let _ = (self.sub_pixel_x, self.sub_pixel_y);
        let r = self.rasterize(index, fx, fy)?;
        let min = (ix + r.offset.0, iy + r.offset.1);
        Some(Glyph {
            dr: (min.0, min.1, min.0 + r.gw, min.1 + r.gh),
            mask: r.mask,
            advance: r.advance,
        })
    }

    /// Port of `face.rasterize`.
    fn rasterize(&mut self, index: Index, fx: Int26_6, fy: Int26_6) -> Option<Rasterized> {
        self.glyph_buf.load(&self.font, self.scale, index).ok()?;
        let b = self.glyph_buf.bounds;
        let xmin = i64::from(fx.wrapping_add(b.min.x)) >> 6;
        let ymin = i64::from(fy.wrapping_sub(b.max.y)) >> 6;
        let xmax = i64::from(fx.wrapping_add(b.max.x).wrapping_add(0x3f)) >> 6;
        let ymax = i64::from(fy.wrapping_sub(b.min.y).wrapping_add(0x3f)) >> 6;
        if xmin > xmax || ymin > ymax {
            return None;
        }
        let fx = fx.wrapping_sub((xmin << 6) as Int26_6);
        let fy = fy.wrapping_sub((ymin << 6) as Int26_6);
        self.r.clear();
        let mut mask = AlphaMask {
            pix: vec![0; (self.maxw.max(0) * self.maxh.max(0)) as usize],
            stride: self.maxw.max(0) as usize,
            width: self.maxw,
            height: self.maxh,
        };
        // Taken out and put back so the contours can be walked while the rasteriser (also in
        // `self`) is fed — no copy of the outline.
        let buf = std::mem::take(&mut self.glyph_buf);
        let mut e0 = 0usize;
        for &e1 in &buf.ends {
            if let Some(points) = buf.points.get(e0..e1) {
                self.draw_contour(points, fx, fy);
            }
            e0 = e1;
        }
        let advance = buf.advance_width;
        self.glyph_buf = buf;
        self.r.rasterize(&mut MaskPainter { mask: &mut mask });
        Some(Rasterized {
            advance,
            offset: (xmin, ymin),
            gw: xmax - xmin,
            gh: ymax - ymin,
            mask,
        })
    }

    /// Port of `face.drawContour`: one closed contour, off-curve points as quadratic controls with
    /// the implied on-curve midpoint between two consecutive off-curve points.
    fn draw_contour(&mut self, ps: &[Point], dx: Int26_6, dy: Int26_6) {
        let Some(first) = ps.first() else {
            return;
        };
        let at = |p: &Point| Point26_6::new(dx.wrapping_add(p.x), dy.wrapping_sub(p.y));
        let mut start = at(first);
        let others: &[Point];
        if on_curve(first) {
            others = &ps[1..];
        } else {
            let last_point = &ps[ps.len() - 1];
            let last = at(last_point);
            if on_curve(last_point) {
                start = last;
                others = &ps[..ps.len() - 1];
            } else {
                start = Point26_6::new(
                    start.x.wrapping_add(last.x) / 2,
                    start.y.wrapping_add(last.y) / 2,
                );
                others = ps;
            }
        }
        self.r.start(start);
        let (mut q0, mut on0) = (start, true);
        for p in others {
            let q = at(p);
            let on = on_curve(p);
            if on {
                if on0 {
                    self.r.add1(q);
                } else {
                    self.r.add2(q0, q);
                }
            } else if !on0 {
                let mid = Point26_6::new(q0.x.wrapping_add(q.x) / 2, q0.y.wrapping_add(q.y) / 2);
                self.r.add2(q0, mid);
            }
            q0 = q;
            on0 = on;
        }
        if on0 {
            self.r.add1(start);
        } else {
            self.r.add2(q0, start);
        }
    }
}
