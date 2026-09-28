//! Port of `github.com/golang/freetype/truetype/glyph.go` for `font.HintingNone`.
//!
//! # Only the unhinted path
//!
//! `truetype.Options{}` — what Mattermost's avatar passes — hints nothing: `Options.hinting()` is
//! `HintingNone` unless `Hinting` is `Vertical` or `Full`. With no hinting, `GlyphBuf.Load` never
//! calls the bytecode interpreter (`hint.go`, 1,770 lines), keeps no `Unhinted`/`InFontUnits`
//! copies and rounds no phantom point, so none of that is ported. A caller wanting hinting gets
//! [`crate::truetype::FontError::Unsupported`] from [`crate::face::Face::new`], not a silently
//! unhinted glyph.

use crate::fixed::{Int26_6, Rectangle26_6};
use crate::truetype::{Font, FontError, Index, LocaFormat, u8_at, u16_at, u32_at};

/// `truetype.Point`: a point in 26.6, with the TrueType flags (bit 0: on the curve).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: Int26_6,
    pub y: Int26_6,
    pub flags: u32,
}

const FLAG_ON_CURVE: u32 = 1;
const FLAG_X_SHORT_VECTOR: u32 = 1 << 1;
const FLAG_Y_SHORT_VECTOR: u32 = 1 << 2;
const FLAG_REPEAT: u32 = 1 << 3;
const FLAG_POSITIVE_X_SHORT_VECTOR: u32 = 1 << 4;
const FLAG_POSITIVE_Y_SHORT_VECTOR: u32 = 1 << 5;
const FLAG_TOUCHED_X: u32 = 1 << 6;
const FLAG_TOUCHED_Y: u32 = 1 << 7;
const FLAG_THIS_X_IS_SAME: u32 = FLAG_POSITIVE_X_SHORT_VECTOR;
const FLAG_THIS_Y_IS_SAME: u32 = FLAG_POSITIVE_Y_SHORT_VECTOR;

/// Whether a point is on the curve (`Flags&0x01 != 0`).
#[must_use]
pub fn on_curve(p: &Point) -> bool {
    p.flags & FLAG_ON_CURVE != 0
}

/// `truetype.GlyphBuf`, unhinted: one glyph's outline, loaded and scaled.
#[derive(Clone, Debug, Default)]
pub struct GlyphBuf {
    /// `AdvanceWidth`.
    pub advance_width: Int26_6,
    /// `Bounds`, y up.
    pub bounds: Rectangle26_6,
    /// `Points`.
    pub points: Vec<Point>,
    /// `Ends`: one past the last point of each contour.
    pub ends: Vec<usize>,
    scale: Int26_6,
    phantom_points: [Point; 4],
    pp1x: Int26_6,
    metrics_set: bool,
}

/// `loadOffset`: past `numberOfContours` and the four bounds.
const LOAD_OFFSET: usize = 10;

impl GlyphBuf {
    /// Port of `GlyphBuf.Load` with `h == font.HintingNone`.
    pub fn load(&mut self, f: &Font, scale: Int26_6, i: Index) -> Result<(), FontError> {
        self.points.clear();
        self.ends.clear();
        self.scale = scale;
        self.pp1x = 0;
        self.phantom_points = [Point::default(); 4];
        self.metrics_set = false;

        self.load_glyph(f, 0, i, true)?;

        let pp1x = self.pp1x;
        if pp1x != 0 {
            for p in &mut self.points {
                p.x = p.x.wrapping_sub(pp1x);
            }
        }
        self.advance_width = self.phantom_points[1]
            .x
            .wrapping_sub(self.phantom_points[0].x);

        match self.points.split_first() {
            None => self.bounds = Rectangle26_6::default(),
            Some((first, rest)) => {
                let mut b = Rectangle26_6::default();
                b.min.x = first.x;
                b.max.x = first.x;
                b.min.y = first.y;
                b.max.y = first.y;
                // Go's `else if`: a point that lowers the minimum is not also tested against the
                // maximum. It cannot matter — the maximum is at least the minimum — but it is the
                // shape of the original.
                for p in rest {
                    if b.min.x > p.x {
                        b.min.x = p.x;
                    } else if b.max.x < p.x {
                        b.max.x = p.x;
                    }
                    if b.min.y > p.y {
                        b.min.y = p.y;
                    } else if b.max.y < p.y {
                        b.max.y = p.y;
                    }
                }
                self.bounds = b;
            }
        }
        Ok(())
    }

    /// Port of `GlyphBuf.load`.
    fn load_glyph(
        &mut self,
        f: &Font,
        recursion: u32,
        i: Index,
        use_my_metrics: bool,
    ) -> Result<(), FontError> {
        if recursion >= 32 {
            return Err(FontError::Unsupported(
                "excessive compound glyph recursion".to_owned(),
            ));
        }
        let (g0, g1) = match f.loca_offset_format {
            LocaFormat::Short => (
                2 * u32::from(u16_at(&f.loca, 2 * usize::from(i))?),
                2 * u32::from(u16_at(&f.loca, 2 * usize::from(i) + 2)?),
            ),
            LocaFormat::Long => (
                u32_at(&f.loca, 4 * usize::from(i))?,
                u32_at(&f.loca, 4 * usize::from(i) + 4)?,
            ),
        };

        let mut glyf: &[u8] = &[];
        let (mut ne, mut bounds_x_min, mut bounds_y_max) = (0i64, 0 as Int26_6, 0 as Int26_6);
        if g0.wrapping_add(10) <= g1 {
            glyf = f
                .glyf
                .get(g0 as usize..g1 as usize)
                .ok_or_else(|| FontError::Format("index out of range".to_owned()))?;
            ne = i64::from(u16_at(glyf, 0)? as i16);
            bounds_x_min = Int26_6::from(u16_at(glyf, 2)? as i16);
            bounds_y_max = Int26_6::from(u16_at(glyf, 8)? as i16);
        }

        let uhm = f.unscaled_hmetric(i)?;
        let uvm = f.unscaled_vmetric(i, bounds_y_max)?;
        let lsb_origin = bounds_x_min.wrapping_sub(uhm.left_side_bearing);
        self.phantom_points = [
            Point {
                x: lsb_origin,
                ..Point::default()
            },
            Point {
                x: lsb_origin.wrapping_add(uhm.advance_width),
                ..Point::default()
            },
            Point {
                x: uhm.advance_width / 2,
                y: bounds_y_max.wrapping_add(uvm.top_side_bearing),
                flags: 0,
            },
            Point {
                x: uhm.advance_width / 2,
                y: bounds_y_max
                    .wrapping_add(uvm.top_side_bearing)
                    .wrapping_sub(uvm.advance_height),
                flags: 0,
            },
        ];

        if glyf.is_empty() {
            let n = self.points.len();
            self.add_phantoms_and_scale(f, n);
            let len = self.points.len();
            self.phantom_points.copy_from_slice(&self.points[len - 4..]);
            self.points.truncate(len - 4);
            return Ok(());
        }

        let pp1x;
        if ne < 0 {
            if ne != -1 {
                return Err(FontError::Unsupported(
                    "negative number of contours".to_owned(),
                ));
            }
            pp1x = f.scale(self.scale.wrapping_mul(lsb_origin));
            self.load_compound(f, recursion, glyf, use_my_metrics)?;
        } else {
            let np0 = self.points.len();
            let ne0 = self.ends.len();
            self.load_simple(glyf, ne)?;
            self.add_phantoms_and_scale(f, np0);
            let len = self.points.len();
            pp1x = self.points[len - 4].x;
            if use_my_metrics {
                self.phantom_points.copy_from_slice(&self.points[len - 4..]);
            }
            self.points.truncate(len - 4);
            if np0 != 0 {
                for e in &mut self.ends[ne0..] {
                    *e += np0;
                }
            }
        }
        if use_my_metrics && !self.metrics_set {
            self.metrics_set = true;
            self.pp1x = pp1x;
        }
        Ok(())
    }

    /// Port of `GlyphBuf.loadSimple`: the contour ends, the flags (with repeats), then the
    /// delta-coded x and y coordinates, all in font units. The instructions are skipped.
    fn load_simple(&mut self, glyf: &[u8], ne: i64) -> Result<(), FontError> {
        let mut offset = LOAD_OFFSET;
        let ne0 = self.ends.len();
        for _ in 0..ne {
            self.ends.push(1 + usize::from(u16_at(glyf, offset)?));
            offset += 2;
        }
        let instr_len = usize::from(u16_at(glyf, offset)?);
        offset += 2 + instr_len;
        if ne == 0 {
            return Ok(());
        }
        let np0 = self.points.len();
        let np1 = np0 + self.ends.last().copied().unwrap_or(0);
        // Go reads the ends it just appended without adding np0; the caller shifts them after.
        let _ = ne0;

        let mut i = np0;
        while i < np1 {
            let c = u32::from(u8_at(glyf, offset)?);
            offset += 1;
            self.points.push(Point {
                flags: c,
                ..Point::default()
            });
            i += 1;
            if c & FLAG_REPEAT != 0 {
                let mut count = u8_at(glyf, offset)?;
                offset += 1;
                while count > 0 {
                    self.points.push(Point {
                        flags: c,
                        ..Point::default()
                    });
                    i += 1;
                    count -= 1;
                }
            }
        }

        let mut x: i16 = 0;
        for i in np0..np1 {
            let flags = self.points.get(i).map(|p| p.flags).ok_or_else(oob)?;
            if flags & FLAG_X_SHORT_VECTOR != 0 {
                let dx = i16::from(u8_at(glyf, offset)?);
                offset += 1;
                if flags & FLAG_POSITIVE_X_SHORT_VECTOR == 0 {
                    x = x.wrapping_sub(dx);
                } else {
                    x = x.wrapping_add(dx);
                }
            } else if flags & FLAG_THIS_X_IS_SAME == 0 {
                x = x.wrapping_add(u16_at(glyf, offset)? as i16);
                offset += 2;
            }
            self.points[i].x = Int26_6::from(x);
        }
        let mut y: i16 = 0;
        for i in np0..np1 {
            let flags = self.points[i].flags;
            if flags & FLAG_Y_SHORT_VECTOR != 0 {
                let dy = i16::from(u8_at(glyf, offset)?);
                offset += 1;
                if flags & FLAG_POSITIVE_Y_SHORT_VECTOR == 0 {
                    y = y.wrapping_sub(dy);
                } else {
                    y = y.wrapping_add(dy);
                }
            } else if flags & FLAG_THIS_Y_IS_SAME == 0 {
                y = y.wrapping_add(u16_at(glyf, offset)? as i16);
                offset += 2;
            }
            self.points[i].y = Int26_6::from(y);
        }
        // A repeat count that overshoots the last contour leaves extra points; Go keeps them.
        Ok(())
    }

    /// Port of `GlyphBuf.loadCompound`: each component loaded, transformed (2.14), offset, and —
    /// unhinted — no instructions run.
    fn load_compound(
        &mut self,
        f: &Font,
        recursion: u32,
        glyf: &[u8],
        use_my_metrics: bool,
    ) -> Result<(), FontError> {
        const ARG_1_AND_2_ARE_WORDS: u16 = 1;
        const ARGS_ARE_XY_VALUES: u16 = 1 << 1;
        const ROUND_XY_TO_GRID: u16 = 1 << 2;
        const WE_HAVE_A_SCALE: u16 = 1 << 3;
        const MORE_COMPONENTS: u16 = 1 << 5;
        const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 1 << 6;
        const WE_HAVE_A_TWO_BY_TWO: u16 = 1 << 7;
        const USE_MY_METRICS: u16 = 1 << 9;

        let np0 = self.points.len();
        let mut offset = LOAD_OFFSET;
        loop {
            let flags = u16_at(glyf, offset)?;
            let component = u16_at(glyf, offset + 2)?;
            let (mut dx, mut dy);
            if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                dx = Int26_6::from(u16_at(glyf, offset + 4)? as i16);
                dy = Int26_6::from(u16_at(glyf, offset + 6)? as i16);
                offset += 8;
            } else {
                dx = Int26_6::from(u8_at(glyf, offset + 4)? as i8);
                dy = Int26_6::from(u8_at(glyf, offset + 5)? as i8);
                offset += 6;
            }
            if flags & ARGS_ARE_XY_VALUES == 0 {
                return Err(FontError::Unsupported(
                    "compound glyph transform vector".to_owned(),
                ));
            }
            let mut transform = [0i16; 4];
            let mut has_transform = false;
            if flags & (WE_HAVE_A_SCALE | WE_HAVE_AN_X_AND_Y_SCALE | WE_HAVE_A_TWO_BY_TWO) != 0 {
                has_transform = true;
                if flags & WE_HAVE_A_SCALE != 0 {
                    transform[0] = u16_at(glyf, offset)? as i16;
                    transform[3] = transform[0];
                    offset += 2;
                } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                    transform[0] = u16_at(glyf, offset)? as i16;
                    transform[3] = u16_at(glyf, offset + 2)? as i16;
                    offset += 4;
                } else {
                    transform[0] = u16_at(glyf, offset)? as i16;
                    transform[1] = u16_at(glyf, offset + 2)? as i16;
                    transform[2] = u16_at(glyf, offset + 4)? as i16;
                    transform[3] = u16_at(glyf, offset + 6)? as i16;
                    offset += 8;
                }
            }
            let saved_pp = self.phantom_points;
            let cnp0 = self.points.len();
            let component_umm = use_my_metrics && (flags & USE_MY_METRICS != 0);
            self.load_glyph(f, recursion + 1, component, component_umm)?;
            if flags & USE_MY_METRICS == 0 {
                self.phantom_points = saved_pp;
            }
            if has_transform {
                for p in &mut self.points[cnp0..] {
                    let t = |v: Int26_6, k: i16| -> Int26_6 {
                        ((i64::from(v) * i64::from(k) + (1 << 13)) >> 14) as Int26_6
                    };
                    let new_x = t(p.x, transform[0]).wrapping_add(t(p.y, transform[2]));
                    let new_y = t(p.x, transform[1]).wrapping_add(t(p.y, transform[3]));
                    p.x = new_x;
                    p.y = new_y;
                }
            }
            dx = f.scale(self.scale.wrapping_mul(dx));
            dy = f.scale(self.scale.wrapping_mul(dy));
            if flags & ROUND_XY_TO_GRID != 0 {
                dx = dx.wrapping_add(32) & !63;
                dy = dy.wrapping_add(32) & !63;
            }
            for p in &mut self.points[cnp0..] {
                p.x = p.x.wrapping_add(dx);
                p.y = p.y.wrapping_add(dy);
            }
            if flags & MORE_COMPONENTS == 0 {
                break;
            }
        }

        // Unhinted: `instrLen` stays 0, so the program is never read.
        let len = self.points.len();
        self.add_phantoms_and_scale(f, len);
        let len = self.points.len();
        let phantoms = [
            self.points[len - 4],
            self.points[len - 3],
            self.points[len - 2],
            self.points[len - 1],
        ];
        self.points.truncate(len - 4);
        for p in &mut self.points[np0..] {
            p.flags &= !(FLAG_TOUCHED_X | FLAG_TOUCHED_Y);
        }
        if !self.metrics_set {
            self.phantom_points = phantoms;
        }
        Ok(())
    }

    /// Port of `GlyphBuf.addPhantomsAndScale` for `HintingNone`: append the four phantom points
    /// and scale every point from `np1` on (the new glyph's own points for a simple glyph, only
    /// the phantoms for a compound one, whose components were scaled as they loaded).
    fn add_phantoms_and_scale(&mut self, f: &Font, np1: usize) {
        self.points.extend_from_slice(&self.phantom_points);
        let scale = self.scale;
        for p in &mut self.points[np1..] {
            p.x = f.scale(scale.wrapping_mul(p.x));
            p.y = f.scale(scale.wrapping_mul(p.y));
        }
    }
}

fn oob() -> FontError {
    FontError::Format("index out of range".to_owned())
}
