//! Port of `github.com/golang/freetype/raster/raster.go`: the anti-aliasing scan converter —
//! `Rasterizer`, its cell list, the line (`Add1`) and quadratic/cubic (`Add2`/`Add3`) edge
//! walkers, and `Rasterize`'s span output.
//!
//! # Integer widths are Go's
//!
//! Go's `int` is 64-bit and `fixed.Int26_6` is 32-bit, and the original mixes them on purpose:
//! `p, q := (64-x0f)*dy, dx` is an `Int26_6` product, while `r.area += int((x0f + x1f) * dy)`
//! multiplies in `Int26_6` **before** widening. Every expression here keeps the width Go computes
//! it in, with `wrapping_*` where Go's `int32` would wrap. Division truncates toward zero in both
//! languages and `%` takes the dividend's sign in both, so `/` and `%` translate directly.
//!
//! `UseNonZeroWinding`, `Dx`/`Dy` and `AddStroke` are kept only as far as the face uses them: the
//! face leaves the first two at their zero values and never strokes.

use crate::fixed::{Int26_6, Point26_6};

/// `raster.Span`: one horizontal run of equal coverage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub y: i64,
    pub x0: i64,
    pub x1: i64,
    pub alpha: u32,
}

/// `raster.Painter`.
pub trait Painter {
    fn paint(&mut self, spans: &[Span], done: bool);
}

#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    xi: i64,
    area: i64,
    cover: i64,
    next: i64,
}

/// `raster.Rasterizer`.
#[derive(Clone, Debug, Default)]
pub struct Rasterizer {
    /// `UseNonZeroWinding`: false is the even-odd rule, which the face uses.
    pub use_non_zero_winding: bool,
    width: i64,
    split_scale2: i64,
    split_scale3: i64,
    a: Point26_6,
    xi: i64,
    yi: i64,
    area: i64,
    cover: i64,
    cell: Vec<Cell>,
    cell_index: Vec<i64>,
    span_buf: Vec<Span>,
}

/// `maxAbs` (geom.go:16).
fn max_abs(a: Int26_6, b: Int26_6) -> Int26_6 {
    let a = a.wrapping_abs();
    let b = b.wrapping_abs();
    if a < b { b } else { a }
}

/// `len(r.spanBuf)` in Go.
const SPAN_BUF_LEN: usize = 64;

impl Rasterizer {
    /// Port of `raster.NewRasterizer`.
    #[must_use]
    pub fn new(width: i64, height: i64) -> Self {
        let mut r = Rasterizer::default();
        r.set_bounds(width, height);
        r
    }

    fn find_cell(&mut self) -> i64 {
        if self.yi < 0 || self.yi >= self.cell_index.len() as i64 {
            return -1;
        }
        let xi = if self.xi < 0 {
            -1
        } else if self.xi > self.width {
            self.width
        } else {
            self.xi
        };
        let (mut i, mut prev) = (self.cell_index[self.yi as usize], -1i64);
        while i != -1 && self.cell[i as usize].xi <= xi {
            if self.cell[i as usize].xi == xi {
                return i;
            }
            prev = i;
            i = self.cell[i as usize].next;
        }
        let c = self.cell.len() as i64;
        self.cell.push(Cell {
            xi,
            area: 0,
            cover: 0,
            next: i,
        });
        if prev == -1 {
            self.cell_index[self.yi as usize] = c;
        } else {
            self.cell[prev as usize].next = c;
        }
        c
    }

    fn save_cell(&mut self) {
        if self.area != 0 || self.cover != 0 {
            let i = self.find_cell();
            if i != -1 {
                self.cell[i as usize].area += self.area;
                self.cell[i as usize].cover += self.cover;
            }
            self.area = 0;
            self.cover = 0;
        }
    }

    fn set_cell(&mut self, xi: i64, yi: i64) {
        if self.xi != xi || self.yi != yi {
            self.save_cell();
            self.xi = xi;
            self.yi = yi;
        }
    }

    fn scan(&mut self, yi: i64, x0: Int26_6, y0f: Int26_6, x1: Int26_6, y1f: Int26_6) {
        let x0i = i64::from(x0) / 64;
        let x0f = x0.wrapping_sub((64 * x0i) as Int26_6);
        let x1i = i64::from(x1) / 64;
        let x1f = x1.wrapping_sub((64 * x1i) as Int26_6);

        if y0f == y1f {
            self.set_cell(x1i, yi);
            return;
        }
        let (dx, dy) = (x1.wrapping_sub(x0), y1f.wrapping_sub(y0f));
        if x0i == x1i {
            self.area += i64::from(x0f.wrapping_add(x1f).wrapping_mul(dy));
            self.cover += i64::from(dy);
            return;
        }
        let (mut p, q, edge0, edge1, xi_delta): (Int26_6, Int26_6, Int26_6, Int26_6, i64) =
            if dx > 0 {
                ((64 - x0f).wrapping_mul(dy), dx, 0, 64, 1)
            } else {
                (x0f.wrapping_mul(dy), dx.wrapping_neg(), 64, 0, -1)
            };
        let (mut y_delta, mut y_rem) = (p.wrapping_div(q), p.wrapping_rem(q));
        if y_rem < 0 {
            y_delta -= 1;
            y_rem += q;
        }
        let (mut xi, mut y) = (x0i, y0f);
        self.area += i64::from(x0f.wrapping_add(edge1).wrapping_mul(y_delta));
        self.cover += i64::from(y_delta);
        xi += xi_delta;
        y = y.wrapping_add(y_delta);
        self.set_cell(xi, yi);
        if xi != x1i {
            p = 64i32.wrapping_mul(y1f.wrapping_sub(y).wrapping_add(y_delta));
            let (mut full_delta, mut full_rem) = (p.wrapping_div(q), p.wrapping_rem(q));
            if full_rem < 0 {
                full_delta -= 1;
                full_rem += q;
            }
            y_rem -= q;
            while xi != x1i {
                y_delta = full_delta;
                y_rem += full_rem;
                if y_rem >= 0 {
                    y_delta += 1;
                    y_rem -= q;
                }
                self.area += i64::from(64i32.wrapping_mul(y_delta));
                self.cover += i64::from(y_delta);
                xi += xi_delta;
                y = y.wrapping_add(y_delta);
                self.set_cell(xi, yi);
            }
        }
        y_delta = y1f.wrapping_sub(y);
        self.area += i64::from(edge0.wrapping_add(x1f).wrapping_mul(y_delta));
        self.cover += i64::from(y_delta);
    }

    /// Port of `Rasterizer.Start`.
    pub fn start(&mut self, a: Point26_6) {
        self.set_cell(i64::from(a.x / 64), i64::from(a.y / 64));
        self.a = a;
    }

    /// Port of `Rasterizer.Add1`: a straight edge to `b`.
    pub fn add1(&mut self, b: Point26_6) {
        let (x0, y0) = (self.a.x, self.a.y);
        let (x1, y1) = (b.x, b.y);
        let (dx, dy) = (x1.wrapping_sub(x0), y1.wrapping_sub(y0));
        let y0i = i64::from(y0) / 64;
        let y0f = y0.wrapping_sub((64 * y0i) as Int26_6);
        let y1i = i64::from(y1) / 64;
        let y1f = y1.wrapping_sub((64 * y1i) as Int26_6);

        if y0i == y1i {
            self.scan(y0i, x0, y0f, x1, y1f);
        } else if dx == 0 {
            let (edge0, edge1, yi_delta): (Int26_6, Int26_6, i64) =
                if dy > 0 { (0, 64, 1) } else { (64, 0, -1) };
            let x0i = i64::from(x0) / 64;
            let mut yi = y0i;
            let x0f_times2 = (i64::from(x0) - 64 * x0i) * 2;
            let mut dcover = i64::from(edge1.wrapping_sub(y0f));
            let mut darea = x0f_times2 * dcover;
            self.area += darea;
            self.cover += dcover;
            yi += yi_delta;
            self.set_cell(x0i, yi);
            dcover = i64::from(edge1.wrapping_sub(edge0));
            darea = x0f_times2 * dcover;
            while yi != y1i {
                self.area += darea;
                self.cover += dcover;
                yi += yi_delta;
                self.set_cell(x0i, yi);
            }
            dcover = i64::from(y1f.wrapping_sub(edge0));
            darea = x0f_times2 * dcover;
            self.area += darea;
            self.cover += dcover;
        } else {
            let (mut p, q, edge0, edge1, yi_delta): (Int26_6, Int26_6, Int26_6, Int26_6, i64) =
                if dy > 0 {
                    ((64 - y0f).wrapping_mul(dx), dy, 0, 64, 1)
                } else {
                    (y0f.wrapping_mul(dx), dy.wrapping_neg(), 64, 0, -1)
                };
            let (mut x_delta, mut x_rem) = (p.wrapping_div(q), p.wrapping_rem(q));
            if x_rem < 0 {
                x_delta -= 1;
                x_rem += q;
            }
            let (mut x, mut yi) = (x0, y0i);
            self.scan(yi, x, y0f, x.wrapping_add(x_delta), edge1);
            x = x.wrapping_add(x_delta);
            yi += yi_delta;
            self.set_cell(i64::from(x) / 64, yi);
            if yi != y1i {
                p = 64i32.wrapping_mul(dx);
                let (mut full_delta, mut full_rem) = (p.wrapping_div(q), p.wrapping_rem(q));
                if full_rem < 0 {
                    full_delta -= 1;
                    full_rem += q;
                }
                x_rem -= q;
                while yi != y1i {
                    x_delta = full_delta;
                    x_rem += full_rem;
                    if x_rem >= 0 {
                        x_delta += 1;
                        x_rem -= q;
                    }
                    self.scan(yi, x, edge0, x.wrapping_add(x_delta), edge1);
                    x = x.wrapping_add(x_delta);
                    yi += yi_delta;
                    self.set_cell(i64::from(x) / 64, yi);
                }
            }
            self.scan(yi, x, edge0, x1, y1f);
        }
        self.a = b;
    }

    /// Port of `Rasterizer.Add2`: a quadratic Bézier with control point `b`, flattened by
    /// de Casteljau subdivision to a depth set by the curve's deviation.
    ///
    /// Go panics past 16 subdivisions ("Add2 nsplit too large"); this caps at 16 instead, which
    /// only a coordinate near `i32::MAX` could reach.
    pub fn add2(&mut self, b: Point26_6, c: Point26_6) {
        let mut dev = max_abs(
            self.a.x.wrapping_sub(b.x.wrapping_mul(2)).wrapping_add(c.x),
            self.a.y.wrapping_sub(b.y.wrapping_mul(2)).wrapping_add(c.y),
        ) / self.split_scale2 as Int26_6;
        let mut nsplit = 0usize;
        while dev > 0 {
            dev /= 4;
            nsplit += 1;
        }
        const MAX_NSPLIT: usize = 16;
        let nsplit = nsplit.min(MAX_NSPLIT);
        let mut p_stack = [Point26_6::default(); 2 * MAX_NSPLIT + 3];
        let mut s_stack = [0usize; MAX_NSPLIT + 1];
        let mut i: i64 = 0;
        s_stack[0] = nsplit;
        p_stack[0] = c;
        p_stack[1] = b;
        p_stack[2] = self.a;
        while i >= 0 {
            let iu = i as usize;
            let s = s_stack[iu];
            let p = &mut p_stack[2 * iu..];
            if s > 0 {
                let mx = p[1].x;
                p[4].x = p[2].x;
                p[3].x = (p[4].x.wrapping_add(mx)) / 2;
                p[1].x = (p[0].x.wrapping_add(mx)) / 2;
                p[2].x = (p[1].x.wrapping_add(p[3].x)) / 2;
                let my = p[1].y;
                p[4].y = p[2].y;
                p[3].y = (p[4].y.wrapping_add(my)) / 2;
                p[1].y = (p[0].y.wrapping_add(my)) / 2;
                p[2].y = (p[1].y.wrapping_add(p[3].y)) / 2;
                s_stack[iu] = s - 1;
                s_stack[iu + 1] = s - 1;
                i += 1;
            } else {
                let midx = (p[0]
                    .x
                    .wrapping_add(p[1].x.wrapping_mul(2))
                    .wrapping_add(p[2].x))
                    / 4;
                let midy = (p[0]
                    .y
                    .wrapping_add(p[1].y.wrapping_mul(2))
                    .wrapping_add(p[2].y))
                    / 4;
                let end = p[0];
                self.add1(Point26_6::new(midx, midy));
                self.add1(end);
                i -= 1;
            }
        }
    }

    /// Port of `Rasterizer.Add3`: a cubic Bézier. TrueType outlines never produce one; kept for
    /// completeness of the rasteriser, capped at 16 subdivisions as [`Rasterizer::add2`] is.
    pub fn add3(&mut self, b: Point26_6, c: Point26_6, d: Point26_6) {
        let mut dev2 = max_abs(
            self.a
                .x
                .wrapping_sub(b.x.wrapping_add(c.x).wrapping_mul(3))
                .wrapping_add(d.x),
            self.a
                .y
                .wrapping_sub(b.y.wrapping_add(c.y).wrapping_mul(3))
                .wrapping_add(d.y),
        ) / self.split_scale2 as Int26_6;
        let mut dev3 = max_abs(
            self.a.x.wrapping_sub(b.x.wrapping_mul(2)).wrapping_add(d.x),
            self.a.y.wrapping_sub(b.y.wrapping_mul(2)).wrapping_add(d.y),
        ) / self.split_scale3 as Int26_6;
        let mut nsplit = 0usize;
        while dev2 > 0 || dev3 > 0 {
            dev2 /= 8;
            dev3 /= 4;
            nsplit += 1;
        }
        const MAX_NSPLIT: usize = 16;
        let nsplit = nsplit.min(MAX_NSPLIT);
        let mut p_stack = [Point26_6::default(); 3 * MAX_NSPLIT + 4];
        let mut s_stack = [0usize; MAX_NSPLIT + 1];
        let mut i: i64 = 0;
        s_stack[0] = nsplit;
        p_stack[0] = d;
        p_stack[1] = c;
        p_stack[2] = b;
        p_stack[3] = self.a;
        while i >= 0 {
            let iu = i as usize;
            let s = s_stack[iu];
            let p = &mut p_stack[3 * iu..];
            if s > 0 {
                let m01x = (p[0].x.wrapping_add(p[1].x)) / 2;
                let m12x = (p[1].x.wrapping_add(p[2].x)) / 2;
                let m23x = (p[2].x.wrapping_add(p[3].x)) / 2;
                p[6].x = p[3].x;
                p[5].x = m23x;
                p[1].x = m01x;
                p[2].x = (m01x.wrapping_add(m12x)) / 2;
                p[4].x = (m12x.wrapping_add(m23x)) / 2;
                p[3].x = (p[2].x.wrapping_add(p[4].x)) / 2;
                let m01y = (p[0].y.wrapping_add(p[1].y)) / 2;
                let m12y = (p[1].y.wrapping_add(p[2].y)) / 2;
                let m23y = (p[2].y.wrapping_add(p[3].y)) / 2;
                p[6].y = p[3].y;
                p[5].y = m23y;
                p[1].y = m01y;
                p[2].y = (m01y.wrapping_add(m12y)) / 2;
                p[4].y = (m12y.wrapping_add(m23y)) / 2;
                p[3].y = (p[2].y.wrapping_add(p[4].y)) / 2;
                s_stack[iu] = s - 1;
                s_stack[iu + 1] = s - 1;
                i += 1;
            } else {
                let midx = (p[0]
                    .x
                    .wrapping_add(p[1].x.wrapping_add(p[2].x).wrapping_mul(3))
                    .wrapping_add(p[3].x))
                    / 8;
                let midy = (p[0]
                    .y
                    .wrapping_add(p[1].y.wrapping_add(p[2].y).wrapping_mul(3))
                    .wrapping_add(p[3].y))
                    / 8;
                let end = p[0];
                self.add1(Point26_6::new(midx, midy));
                self.add1(end);
                i -= 1;
            }
        }
    }

    /// `areaToAlpha`: the even-odd (or non-zero) fold of a cell's signed area into 16-bit alpha.
    fn area_to_alpha(&self, area: i64) -> u32 {
        let mut a = (area + 1) >> 1;
        if a < 0 {
            a = -a;
        }
        // Go converts the `int` to `uint32`, which keeps the low 32 bits.
        let mut alpha = a as u32;
        if self.use_non_zero_winding {
            if alpha > 0x0fff {
                alpha = 0x0fff;
            }
        } else {
            alpha &= 0x1fff;
            if alpha > 0x1000 {
                alpha = 0x2000 - alpha;
            } else if alpha == 0x1000 {
                alpha = 0x0fff;
            }
        }
        alpha << 4 | alpha >> 8
    }

    /// Port of `Rasterizer.Rasterize`: every cell turned into spans, handed to `p` in batches of
    /// at most 62, the last batch with `done`.
    pub fn rasterize(&mut self, p: &mut dyn Painter) {
        self.save_cell();
        self.span_buf.clear();
        for yi in 0..self.cell_index.len() as i64 {
            let (mut xi, mut cover) = (0i64, 0i64);
            let mut c = self.cell_index[yi as usize];
            while c != -1 {
                let cell = self.cell[c as usize];
                if cover != 0 && cell.xi > xi {
                    let alpha = self.area_to_alpha(cover * 64 * 2);
                    if alpha != 0 {
                        let (mut xi0, mut xi1) = (xi, cell.xi);
                        if xi0 < 0 {
                            xi0 = 0;
                        }
                        if xi1 >= self.width {
                            xi1 = self.width;
                        }
                        if xi0 < xi1 {
                            self.span_buf.push(Span {
                                y: yi,
                                x0: xi0,
                                x1: xi1,
                                alpha,
                            });
                        }
                    }
                }
                cover += cell.cover;
                let alpha = self.area_to_alpha(cover * 64 * 2 - cell.area);
                xi = cell.xi + 1;
                if alpha != 0 {
                    let (mut xi0, mut xi1) = (cell.xi, xi);
                    if xi0 < 0 {
                        xi0 = 0;
                    }
                    if xi1 >= self.width {
                        xi1 = self.width;
                    }
                    if xi0 < xi1 {
                        self.span_buf.push(Span {
                            y: yi,
                            x0: xi0,
                            x1: xi1,
                            alpha,
                        });
                    }
                }
                if self.span_buf.len() > SPAN_BUF_LEN - 2 {
                    p.paint(&self.span_buf, false);
                    self.span_buf.clear();
                }
                c = cell.next;
            }
        }
        p.paint(&self.span_buf, true);
        self.span_buf.clear();
    }

    /// Port of `Rasterizer.Clear`.
    pub fn clear(&mut self) {
        self.a = Point26_6::default();
        self.xi = 0;
        self.yi = 0;
        self.area = 0;
        self.cover = 0;
        self.cell.clear();
        self.cell_index.fill(-1);
    }

    /// Port of `Rasterizer.SetBounds`: the width and height of the area to rasterise, and the
    /// flattening tolerances that grow with it.
    pub fn set_bounds(&mut self, width: i64, height: i64) {
        let width = width.max(0);
        let height = height.max(0);
        let (mut ss2, mut ss3) = (32, 16);
        if width > 24 || height > 24 {
            ss2 *= 2;
            ss3 *= 2;
            if width > 120 || height > 120 {
                ss2 *= 2;
                ss3 *= 2;
            }
        }
        self.width = width;
        self.split_scale2 = ss2;
        self.split_scale3 = ss3;
        self.cell = Vec::new();
        self.cell_index = vec![-1; height as usize];
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The even-odd fold: a full cell is `0x0fff` (not `0x1000`), and past a full cell the alpha
    /// folds back down.
    #[test]
    fn area_to_alpha_folds_even_odd() {
        let r = Rasterizer::new(1, 1);
        let full = 64 * 64 * 2;
        assert_eq!(r.area_to_alpha(full), 0x0fff << 4 | 0x0fff >> 8);
        assert_eq!(r.area_to_alpha(0), 0);
        let half = r.area_to_alpha(full / 2);
        assert_eq!(half, 0x0800 << 4 | 0x0800 >> 8);
        // One and a half cells — only overlapping contours get here: 0x1800 folds to 0x0800.
        assert_eq!(r.area_to_alpha(full + full / 2), 0x0800 << 4 | 0x0800 >> 8);
        // Twice full: 0x2000, masked to zero.
        assert_eq!(r.area_to_alpha(2 * full), 0);
    }

    /// The split tolerances double past 24 and again past 120 pixels.
    #[test]
    fn the_split_scales_grow_with_the_bounds() {
        let r = Rasterizer::new(24, 24);
        assert_eq!((r.split_scale2, r.split_scale3), (32, 16));
        let r = Rasterizer::new(25, 1);
        assert_eq!((r.split_scale2, r.split_scale3), (64, 32));
        let r = Rasterizer::new(1, 121);
        assert_eq!((r.split_scale2, r.split_scale3), (128, 64));
    }
}
