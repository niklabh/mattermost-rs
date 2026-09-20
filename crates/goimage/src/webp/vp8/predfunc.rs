//! Port of `golang.org/x/image/vp8/predfunc.go`: the prediction functions of chapter 12.
//!
//! For each macroblock the luma values are predicted either as one 16x16 region or as 16 separate
//! 4x4 regions, and the chroma values always as one 8x8 region. A 4x4 region's predicted values
//! (Xs) are a function of the previously decoded top and left borders plus some pixels from the
//! top-right:
//!
//! ```text
//! a b c d e f g h
//! p X X X X
//! q X X X X
//! r X X X X
//! s X X X X
//! ```
//!
//! Every sum below is Go's: the DC modes accumulate in a `uint32` and truncate the average to
//! `uint8`, the weighted averages divide an `int32` by 4 or 2 (both non-negative, so truncation and
//! flooring agree), and TM clips to `[0, 255]`.

use super::Decoder;
use super::quant::clip;

/// `nPred`: the number of predictor modes, not counting the Top/Left variants of DC.
pub(super) const N_PRED: usize = 10;

pub(super) const PRED_DC: u8 = 0;
pub(super) const PRED_TM: u8 = 1;
pub(super) const PRED_VE: u8 = 2;
pub(super) const PRED_HE: u8 = 3;
pub(super) const PRED_RD: u8 = 4;
pub(super) const PRED_VR: u8 = 5;
pub(super) const PRED_LD: u8 = 6;
pub(super) const PRED_VL: u8 = 7;
pub(super) const PRED_HD: u8 = 8;
pub(super) const PRED_HU: u8 = 9;
pub(super) const PRED_DC_TOP: u8 = 10;
pub(super) const PRED_DC_LEFT: u8 = 11;
pub(super) const PRED_DC_TOP_LEFT: u8 = 12;

/// `checkTopLeftPred`: the DC mode has variants for the top row and the left column.
pub(super) fn check_top_left_pred(mbx: usize, mby: usize, p: u8) -> u8 {
    if p != PRED_DC {
        return p;
    }
    if mbx == 0 {
        if mby == 0 {
            return PRED_DC_TOP_LEFT;
        }
        return PRED_DC_LEFT;
    }
    if mby == 0 {
        return PRED_DC_TOP;
    }
    PRED_DC
}

impl Decoder<'_> {
    /// `predFunc4[p]`. The modes past `PRED_HU` are nil in Go's table and are never selected for a
    /// 4x4 region, since `checkTopLeftPred` is not applied to `predY4`.
    pub(super) fn pred_func4(&mut self, p: u8, y: usize, x: usize) {
        match p {
            PRED_DC => self.pred_func4_dc(y, x),
            PRED_TM => self.pred_func4_tm(y, x),
            PRED_VE => self.pred_func4_ve(y, x),
            PRED_HE => self.pred_func4_he(y, x),
            PRED_RD => self.pred_func4_rd(y, x),
            PRED_VR => self.pred_func4_vr(y, x),
            PRED_LD => self.pred_func4_ld(y, x),
            PRED_VL => self.pred_func4_vl(y, x),
            PRED_HD => self.pred_func4_hd(y, x),
            PRED_HU => self.pred_func4_hu(y, x),
            _ => {}
        }
    }

    /// `predFunc8[p]`.
    pub(super) fn pred_func8(&mut self, p: u8, y: usize, x: usize) {
        match p {
            PRED_DC => self.pred_func_n_dc(8, y, x),
            PRED_TM => self.pred_func_n_tm(8, y, x),
            PRED_VE => self.pred_func_n_ve(8, y, x),
            PRED_HE => self.pred_func_n_he(8, y, x),
            PRED_DC_TOP => self.pred_func_n_dc_top(8, y, x),
            PRED_DC_LEFT => self.pred_func_n_dc_left(8, y, x),
            PRED_DC_TOP_LEFT => self.pred_func_n_dc_top_left(8, y, x),
            _ => {}
        }
    }

    /// `predFunc16[p]`.
    pub(super) fn pred_func16(&mut self, p: u8, y: usize, x: usize) {
        match p {
            PRED_DC => self.pred_func_n_dc(16, y, x),
            PRED_TM => self.pred_func_n_tm(16, y, x),
            PRED_VE => self.pred_func_n_ve(16, y, x),
            PRED_HE => self.pred_func_n_he(16, y, x),
            PRED_DC_TOP => self.pred_func_n_dc_top(16, y, x),
            PRED_DC_LEFT => self.pred_func_n_dc_left(16, y, x),
            PRED_DC_TOP_LEFT => self.pred_func_n_dc_top_left(16, y, x),
            _ => {}
        }
    }

    // --- 8x8 and 16x16 ---------------------------------------------------------------------------
    //
    // `predFunc8*` and `predFunc16*` differ only in `n` and in the rounding constant, which is
    // always `n/2` for DC and `n/4` for the Top and Left variants — the same `sum / (2n)` and
    // `sum / n` averages Go writes out twice.

    /// `predFunc8DC` / `predFunc16DC`.
    fn pred_func_n_dc(&mut self, n: usize, y: usize, x: usize) {
        let mut sum = n as u32;
        for i in 0..n {
            sum += u32::from(self.ybr[y - 1][x + i]);
        }
        for j in 0..n {
            sum += u32::from(self.ybr[y + j][x - 1]);
        }
        let avg = (sum / (2 * n as u32)) as u8;
        self.fill(n, y, x, avg);
    }

    /// `predFunc8TM` / `predFunc16TM`.
    fn pred_func_n_tm(&mut self, n: usize, y: usize, x: usize) {
        let delta0 = -i32::from(self.ybr[y - 1][x - 1]);
        for j in 0..n {
            let delta1 = delta0 + i32::from(self.ybr[y + j][x - 1]);
            for i in 0..n {
                let delta2 = delta1 + i32::from(self.ybr[y - 1][x + i]);
                self.ybr[y + j][x + i] = clip(delta2, 0, 255) as u8;
            }
        }
    }

    /// `predFunc8VE` / `predFunc16VE`.
    fn pred_func_n_ve(&mut self, n: usize, y: usize, x: usize) {
        for j in 0..n {
            for i in 0..n {
                self.ybr[y + j][x + i] = self.ybr[y - 1][x + i];
            }
        }
    }

    /// `predFunc8HE` / `predFunc16HE`.
    fn pred_func_n_he(&mut self, n: usize, y: usize, x: usize) {
        for j in 0..n {
            for i in 0..n {
                self.ybr[y + j][x + i] = self.ybr[y + j][x - 1];
            }
        }
    }

    /// `predFunc8DCTop` / `predFunc16DCTop`: no row above, so only the left column counts.
    fn pred_func_n_dc_top(&mut self, n: usize, y: usize, x: usize) {
        let mut sum = n as u32 / 2;
        for j in 0..n {
            sum += u32::from(self.ybr[y + j][x - 1]);
        }
        let avg = (sum / n as u32) as u8;
        self.fill(n, y, x, avg);
    }

    /// `predFunc8DCLeft` / `predFunc16DCLeft`: no column to the left, so only the top row counts.
    fn pred_func_n_dc_left(&mut self, n: usize, y: usize, x: usize) {
        let mut sum = n as u32 / 2;
        for i in 0..n {
            sum += u32::from(self.ybr[y - 1][x + i]);
        }
        let avg = (sum / n as u32) as u8;
        self.fill(n, y, x, avg);
    }

    /// `predFunc8DCTopLeft` / `predFunc16DCTopLeft`.
    fn pred_func_n_dc_top_left(&mut self, n: usize, y: usize, x: usize) {
        self.fill(n, y, x, 0x80);
    }

    fn fill(&mut self, n: usize, y: usize, x: usize, v: u8) {
        for j in 0..n {
            for i in 0..n {
                self.ybr[y + j][x + i] = v;
            }
        }
    }

    // --- 4x4 -------------------------------------------------------------------------------------

    /// `predFunc4DC`.
    fn pred_func4_dc(&mut self, y: usize, x: usize) {
        let mut sum = 4u32;
        for i in 0..4 {
            sum += u32::from(self.ybr[y - 1][x + i]);
        }
        for j in 0..4 {
            sum += u32::from(self.ybr[y + j][x - 1]);
        }
        let avg = (sum / 8) as u8;
        self.fill(4, y, x, avg);
    }

    /// `predFunc4TM`.
    fn pred_func4_tm(&mut self, y: usize, x: usize) {
        let delta0 = -i32::from(self.ybr[y - 1][x - 1]);
        for j in 0..4 {
            let delta1 = delta0 + i32::from(self.ybr[y + j][x - 1]);
            for i in 0..4 {
                let delta2 = delta1 + i32::from(self.ybr[y - 1][x + i]);
                self.ybr[y + j][x + i] = clip(delta2, 0, 255) as u8;
            }
        }
    }

    /// `predFunc4VE`.
    fn pred_func4_ve(&mut self, y: usize, x: usize) {
        let a = i32::from(self.ybr[y - 1][x - 1]);
        let b = i32::from(self.ybr[y - 1][x]);
        let c = i32::from(self.ybr[y - 1][x + 1]);
        let d = i32::from(self.ybr[y - 1][x + 2]);
        let e = i32::from(self.ybr[y - 1][x + 3]);
        let f = i32::from(self.ybr[y - 1][x + 4]);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        let cde = w121(c, d, e);
        let def = w121(d, e, f);
        for j in 0..4 {
            self.ybr[y + j][x] = abc;
            self.ybr[y + j][x + 1] = bcd;
            self.ybr[y + j][x + 2] = cde;
            self.ybr[y + j][x + 3] = def;
        }
    }

    /// `predFunc4HE`.
    fn pred_func4_he(&mut self, y: usize, x: usize) {
        let s = i32::from(self.ybr[y + 3][x - 1]);
        let r = i32::from(self.ybr[y + 2][x - 1]);
        let q = i32::from(self.ybr[y + 1][x - 1]);
        let p = i32::from(self.ybr[y][x - 1]);
        let a = i32::from(self.ybr[y - 1][x - 1]);
        let ssr = w121(s, s, r);
        let srq = w121(s, r, q);
        let rqp = w121(r, q, p);
        let apq = w121(a, p, q);
        for i in 0..4 {
            self.ybr[y][x + i] = apq;
            self.ybr[y + 1][x + i] = rqp;
            self.ybr[y + 2][x + i] = srq;
            self.ybr[y + 3][x + i] = ssr;
        }
    }

    /// `predFunc4RD`.
    fn pred_func4_rd(&mut self, y: usize, x: usize) {
        let s = i32::from(self.ybr[y + 3][x - 1]);
        let r = i32::from(self.ybr[y + 2][x - 1]);
        let q = i32::from(self.ybr[y + 1][x - 1]);
        let p = i32::from(self.ybr[y][x - 1]);
        let a = i32::from(self.ybr[y - 1][x - 1]);
        let b = i32::from(self.ybr[y - 1][x]);
        let c = i32::from(self.ybr[y - 1][x + 1]);
        let d = i32::from(self.ybr[y - 1][x + 2]);
        let e = i32::from(self.ybr[y - 1][x + 3]);
        let srq = w121(s, r, q);
        let rqp = w121(r, q, p);
        let qpa = w121(q, p, a);
        let pab = w121(p, a, b);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        let cde = w121(c, d, e);
        self.put4(y, x, [pab, abc, bcd, cde]);
        self.put4(y + 1, x, [qpa, pab, abc, bcd]);
        self.put4(y + 2, x, [rqp, qpa, pab, abc]);
        self.put4(y + 3, x, [srq, rqp, qpa, pab]);
    }

    /// `predFunc4VR`.
    fn pred_func4_vr(&mut self, y: usize, x: usize) {
        let r = i32::from(self.ybr[y + 2][x - 1]);
        let q = i32::from(self.ybr[y + 1][x - 1]);
        let p = i32::from(self.ybr[y][x - 1]);
        let a = i32::from(self.ybr[y - 1][x - 1]);
        let b = i32::from(self.ybr[y - 1][x]);
        let c = i32::from(self.ybr[y - 1][x + 1]);
        let d = i32::from(self.ybr[y - 1][x + 2]);
        let e = i32::from(self.ybr[y - 1][x + 3]);
        let ab = w11(a, b);
        let bc = w11(b, c);
        let cd = w11(c, d);
        let de = w11(d, e);
        let rqp = w121(r, q, p);
        let qpa = w121(q, p, a);
        let pab = w121(p, a, b);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        let cde = w121(c, d, e);
        self.put4(y, x, [ab, bc, cd, de]);
        self.put4(y + 1, x, [pab, abc, bcd, cde]);
        self.put4(y + 2, x, [qpa, ab, bc, cd]);
        self.put4(y + 3, x, [rqp, pab, abc, bcd]);
    }

    /// `predFunc4LD`.
    fn pred_func4_ld(&mut self, y: usize, x: usize) {
        let a = i32::from(self.ybr[y - 1][x]);
        let b = i32::from(self.ybr[y - 1][x + 1]);
        let c = i32::from(self.ybr[y - 1][x + 2]);
        let d = i32::from(self.ybr[y - 1][x + 3]);
        let e = i32::from(self.ybr[y - 1][x + 4]);
        let f = i32::from(self.ybr[y - 1][x + 5]);
        let g = i32::from(self.ybr[y - 1][x + 6]);
        let h = i32::from(self.ybr[y - 1][x + 7]);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        let cde = w121(c, d, e);
        let def = w121(d, e, f);
        let efg = w121(e, f, g);
        let fgh = w121(f, g, h);
        let ghh = w121(g, h, h);
        self.put4(y, x, [abc, bcd, cde, def]);
        self.put4(y + 1, x, [bcd, cde, def, efg]);
        self.put4(y + 2, x, [cde, def, efg, fgh]);
        self.put4(y + 3, x, [def, efg, fgh, ghh]);
    }

    /// `predFunc4VL`.
    fn pred_func4_vl(&mut self, y: usize, x: usize) {
        let a = i32::from(self.ybr[y - 1][x]);
        let b = i32::from(self.ybr[y - 1][x + 1]);
        let c = i32::from(self.ybr[y - 1][x + 2]);
        let d = i32::from(self.ybr[y - 1][x + 3]);
        let e = i32::from(self.ybr[y - 1][x + 4]);
        let f = i32::from(self.ybr[y - 1][x + 5]);
        let g = i32::from(self.ybr[y - 1][x + 6]);
        let h = i32::from(self.ybr[y - 1][x + 7]);
        let ab = w11(a, b);
        let bc = w11(b, c);
        let cd = w11(c, d);
        let de = w11(d, e);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        let cde = w121(c, d, e);
        let def = w121(d, e, f);
        let efg = w121(e, f, g);
        let fgh = w121(f, g, h);
        self.put4(y, x, [ab, bc, cd, de]);
        self.put4(y + 1, x, [abc, bcd, cde, def]);
        self.put4(y + 2, x, [bc, cd, de, efg]);
        self.put4(y + 3, x, [bcd, cde, def, fgh]);
    }

    /// `predFunc4HD`.
    fn pred_func4_hd(&mut self, y: usize, x: usize) {
        let s = i32::from(self.ybr[y + 3][x - 1]);
        let r = i32::from(self.ybr[y + 2][x - 1]);
        let q = i32::from(self.ybr[y + 1][x - 1]);
        let p = i32::from(self.ybr[y][x - 1]);
        let a = i32::from(self.ybr[y - 1][x - 1]);
        let b = i32::from(self.ybr[y - 1][x]);
        let c = i32::from(self.ybr[y - 1][x + 1]);
        let d = i32::from(self.ybr[y - 1][x + 2]);
        let sr = w11(s, r);
        let rq = w11(r, q);
        let qp = w11(q, p);
        let pa = w11(p, a);
        let srq = w121(s, r, q);
        let rqp = w121(r, q, p);
        let qpa = w121(q, p, a);
        let pab = w121(p, a, b);
        let abc = w121(a, b, c);
        let bcd = w121(b, c, d);
        self.put4(y, x, [pa, pab, abc, bcd]);
        self.put4(y + 1, x, [qp, qpa, pa, pab]);
        self.put4(y + 2, x, [rq, rqp, qp, qpa]);
        self.put4(y + 3, x, [sr, srq, rq, rqp]);
    }

    /// `predFunc4HU`.
    fn pred_func4_hu(&mut self, y: usize, x: usize) {
        let s = i32::from(self.ybr[y + 3][x - 1]);
        let r = i32::from(self.ybr[y + 2][x - 1]);
        let q = i32::from(self.ybr[y + 1][x - 1]);
        let p = i32::from(self.ybr[y][x - 1]);
        let pq = w11(p, q);
        let qr = w11(q, r);
        let rs = w11(r, s);
        let pqr = w121(p, q, r);
        let qrs = w121(q, r, s);
        let rss = w121(r, s, s);
        let sss = s as u8;
        self.put4(y, x, [pq, pqr, qr, qrs]);
        self.put4(y + 1, x, [qr, qrs, rs, rss]);
        self.put4(y + 2, x, [rs, rss, sss, sss]);
        self.put4(y + 3, x, [sss, sss, sss, sss]);
    }

    fn put4(&mut self, y: usize, x: usize, v: [u8; 4]) {
        self.ybr[y][x..x + 4].copy_from_slice(&v);
    }
}

/// The `(a + 2*b + c + 2) / 4` three-tap smoothing every diagonal mode is built from.
fn w121(a: i32, b: i32, c: i32) -> u8 {
    ((a + 2 * b + c + 2) / 4) as u8
}

/// The `(a + b + 1) / 2` two-tap average the diagonal modes use for their half-pixel rows.
fn w11(a: i32, b: i32) -> u8 {
    ((a + b + 1) / 2) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smoothing_taps_round_as_go_does() {
        assert_eq!(w121(0, 0, 0), 0);
        assert_eq!(w121(255, 255, 255), 255);
        // (1 + 2*1 + 2 + 2) / 4 = 7 / 4 = 1.
        assert_eq!(w121(1, 1, 2), 1);
        // (1 + 2*2 + 2 + 2) / 4 = 9 / 4 = 2.
        assert_eq!(w121(1, 2, 2), 2);
        // `(a + b + 1) / 2` rounds up, so an odd sum lands on the higher of the two.
        assert_eq!(w11(0, 1), 1);
        assert_eq!(w11(1, 2), 2);
        assert_eq!(w11(2, 3), 3);
        assert_eq!(w11(0, 0), 0);
        assert_eq!(w11(255, 255), 255);
    }

    #[test]
    fn check_top_left_pred_only_rewrites_dc() {
        assert_eq!(check_top_left_pred(0, 0, PRED_DC), PRED_DC_TOP_LEFT);
        assert_eq!(check_top_left_pred(0, 1, PRED_DC), PRED_DC_LEFT);
        assert_eq!(check_top_left_pred(1, 0, PRED_DC), PRED_DC_TOP);
        assert_eq!(check_top_left_pred(1, 1, PRED_DC), PRED_DC);
        for p in [PRED_TM, PRED_VE, PRED_HE, PRED_HU] {
            assert_eq!(check_top_left_pred(0, 0, p), p);
        }
    }
}
