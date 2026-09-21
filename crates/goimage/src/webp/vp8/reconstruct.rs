//! Port of `golang.org/x/image/vp8/reconstruct.go`: decoding the DCT/WHT residual coefficients and
//! reconstructing YCbCr data equal to the predicted values plus the residuals.
//!
//! There are `1*16*16 + 2*8*8 + 1*4*4` coefficients per macroblock — luma DCT, two chroma DCT
//! planes and the luma WHT block — read in lots of sixteen whose later entries are usually zero.
//!
//! The YCbCr workspace is `[1+16+1+8][32]uint8`, so vertically adjacent samples are 32 bytes apart:
//!
//! ```text
//! 0 1 2 3 4 5 6 7  8 9 0 1 2 3 4 5  6 7 8 9 0 1 2 3  4 5 6 7 8 9 0 1
//! . . . . . . . a  b b b b b b b b  b b b b b b b b  c c c c . . . .   0
//! . . . . . . . d  Y Y Y Y Y Y Y Y  Y Y Y Y Y Y Y Y  . . . . . . . .   1..16
//! . . . . . . . e  f f f f f f f f  . . . . . . . g  h h h h h h h h   17
//! . . . . . . . i  B B B B B B B B  . . . . . . . j  R R R R R R R R   18..25
//! ```
//!
//! For an uppermost macroblock the `{abcefgh}` values are 0x7f and for a leftmost one the
//! `{adeigj}` values are 0x81 — note that Go's comment at the head of reconstruct.go has those two
//! constants the other way round; the code below follows the code, and the parity corpus agrees
//! with the code.

use super::idct::clip8;
use super::partition::UNIFORM_PROB;
use super::predfunc::check_top_left_pred;
use super::token::{PLANE_UV, PLANE_Y1_SANS_Y2, PLANE_Y1_WITH_Y2, PLANE_Y2};
use super::{Decoder, Mb};

// Spelled as Go spells them — `1*16*16 + 0*8*8`, `1*16*16 + 1*8*8`, `1*16*16 + 2*8*8` — so the
// luma, chroma and WHT shares of the workspace stay visible.
pub(super) const B_COEFF_BASE: usize = 16 * 16;
pub(super) const R_COEFF_BASE: usize = 16 * 16 + 8 * 8;
pub(super) const WHT_COEFF_BASE: usize = 16 * 16 + 2 * 8 * 8;

pub(super) const YBR_YX: usize = 8;
pub(super) const YBR_YY: usize = 1;
pub(super) const YBR_BX: usize = 8;
pub(super) const YBR_BY: usize = 18;
pub(super) const YBR_RX: usize = 24;
pub(super) const YBR_RY: usize = 18;

/// `btou`.
pub(super) fn btou(b: bool) -> u8 {
    u8::from(b)
}

/// `pack`: four 0/1 values into four bits of a `uint32`.
fn pack(x: [u8; 4], shift: u32) -> u32 {
    let u = u32::from(x[0]) | u32::from(x[1]) << 1 | u32::from(x[2]) << 2 | u32::from(x[3]) << 3;
    u << shift
}

/// `unpack`: four 0/1 values from a four-bit value.
fn unpack(v: u8) -> [u8; 4] {
    [v & 1, (v >> 1) & 1, (v >> 2) & 1, (v >> 3) & 1]
}

/// `bands`: the mapping from 4x4 region position to band, specified in section 13.3.
const BANDS: [usize; 17] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7, 0];

/// `cat3456`: the category probabilities of section 13.2. Categories 1 and 2 are decoded inline.
const CAT3456: [[u8; 12]; 4] = [
    [173, 148, 140, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [176, 155, 140, 135, 0, 0, 0, 0, 0, 0, 0, 0],
    [180, 157, 141, 134, 130, 0, 0, 0, 0, 0, 0, 0],
    [254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129, 0],
];

/// `zigzag`:
///
/// ```text
/// 0  1  5  6
/// 2  4  7 12
/// 3  8 11 13
/// 9 10 14 15
/// ```
const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

impl Decoder<'_> {
    /// `Decoder.parseResiduals4`: one 4x4 region of residual coefficients (section 13.3),
    /// returning a 0/1 value saying whether any coefficient was non-zero.
    ///
    /// `plane` and `context` pick the token probability table; `context` is 0, 1 or 2 and counts
    /// how many of the macroblock left and the macroblock above have non-zero coefficients.
    fn parse_residuals4(
        &mut self,
        partition: usize,
        plane: usize,
        context: u8,
        quant: [u16; 2],
        skip_first_coeff: bool,
        coeff_base: usize,
    ) -> u8 {
        let prob = &self.token_prob[plane];
        let mut n = if skip_first_coeff { 1usize } else { 0 };
        let r = &mut self.op[partition];
        let mut p = prob[BANDS[n]][context as usize];
        if !r.read_bit(p[0]) {
            return 0;
        }
        while n != 16 {
            n += 1;
            if !r.read_bit(p[1]) {
                p = prob[BANDS[n]][0];
                continue;
            }
            let v: u32;
            if !r.read_bit(p[2]) {
                v = 1;
                p = prob[BANDS[n]][1];
            } else {
                if !r.read_bit(p[3]) {
                    if !r.read_bit(p[4]) {
                        v = 2;
                    } else {
                        v = 3 + r.read_uint(p[5], 1);
                    }
                } else if !r.read_bit(p[6]) {
                    if !r.read_bit(p[7]) {
                        // Category 1.
                        v = 5 + r.read_uint(159, 1);
                    } else {
                        // Category 2.
                        v = 7 + 2 * r.read_uint(165, 1) + r.read_uint(145, 1);
                    }
                } else {
                    // Categories 3, 4, 5 or 6.
                    let b1 = r.read_uint(p[8], 1);
                    let b0 = r.read_uint(p[9 + b1 as usize], 1);
                    let cat = (2 * b1 + b0) as usize;
                    let tab = &CAT3456[cat];
                    let mut w = 0u32;
                    let mut i = 0;
                    while tab[i] != 0 {
                        w *= 2;
                        w += r.read_uint(tab[i], 1);
                        i += 1;
                    }
                    v = w + 3 + (8 << cat);
                }
                p = prob[BANDS[n]][2];
            }
            let z = ZIGZAG[n - 1];
            // Go computes `int32(v) * int32(quant[...])` and truncates to int16; the product can
            // exceed an int16 and the truncation is part of the format.
            let mut c = (v as i32).wrapping_mul(i32::from(quant[usize::from(btou(z > 0))]));
            if r.read_bit(UNIFORM_PROB) {
                c = -c;
            }
            self.coeff[coeff_base + z] = c as i16;
            if n == 16 || !r.read_bit(p[0]) {
                return 1;
            }
        }
        1
    }

    /// `Decoder.parseResiduals`: the residuals, returning whether inner loop filtering should be
    /// skipped for this macroblock.
    fn parse_residuals(&mut self, mbx: usize, _mby: usize, mby: usize) -> bool {
        let partition = mby & (self.n_op - 1);
        let mut plane = PLANE_Y1_SANS_Y2;
        let quant = self.quant[self.segment];

        // Parse the DC coefficient of each 4x4 luma region.
        if self.use_pred_y16 {
            let context = self.left_mb.nz_y16 + self.up_mb[mbx].nz_y16;
            let nz = self.parse_residuals4(
                partition,
                PLANE_Y2,
                context,
                quant.y2,
                false,
                WHT_COEFF_BASE,
            );
            self.left_mb.nz_y16 = nz;
            self.up_mb[mbx].nz_y16 = nz;
            self.inverse_wht16();
            plane = PLANE_Y1_WITH_Y2;
        }

        let mut nz_dc = [0u8; 4];
        let mut nz_ac = [0u8; 4];
        let mut nz_dc_mask = 0u32;
        let mut nz_ac_mask = 0u32;
        let mut coeff_base = 0usize;

        // Parse the luma coefficients.
        let mut lnz = unpack(self.left_mb.nz_mask & 0x0f);
        let mut unz = unpack(self.up_mb[mbx].nz_mask & 0x0f);
        #[allow(clippy::needless_range_loop)]
        for y in 0..4 {
            let mut nz = lnz[y];
            for x in 0..4 {
                nz = self.parse_residuals4(
                    partition,
                    plane,
                    nz + unz[x],
                    quant.y1,
                    self.use_pred_y16,
                    coeff_base,
                );
                unz[x] = nz;
                nz_ac[x] = nz;
                nz_dc[x] = btou(self.coeff[coeff_base] != 0);
                coeff_base += 16;
            }
            lnz[y] = nz;
            nz_dc_mask |= pack(nz_dc, y as u32 * 4);
            nz_ac_mask |= pack(nz_ac, y as u32 * 4);
        }
        let mut lnz_mask = pack(lnz, 0);
        let mut unz_mask = pack(unz, 0);

        // Parse the chroma coefficients.
        lnz = unpack(self.left_mb.nz_mask >> 4);
        unz = unpack(self.up_mb[mbx].nz_mask >> 4);
        for c in (0..4).step_by(2) {
            for y in 0..2 {
                let mut nz = lnz[y + c];
                for x in 0..2 {
                    nz = self.parse_residuals4(
                        partition,
                        PLANE_UV,
                        nz + unz[x + c],
                        quant.uv,
                        false,
                        coeff_base,
                    );
                    unz[x + c] = nz;
                    nz_ac[y * 2 + x] = nz;
                    nz_dc[y * 2 + x] = btou(self.coeff[coeff_base] != 0);
                    coeff_base += 16;
                }
                lnz[y + c] = nz;
            }
            nz_dc_mask |= pack(nz_dc, 16 + c as u32 * 2);
            nz_ac_mask |= pack(nz_ac, 16 + c as u32 * 2);
        }
        lnz_mask |= pack(lnz, 4);
        unz_mask |= pack(unz, 4);

        // Save decoder state.
        self.left_mb.nz_mask = lnz_mask as u8;
        self.up_mb[mbx].nz_mask = unz_mask as u8;
        self.nz_dc_mask = nz_dc_mask;
        self.nz_ac_mask = nz_ac_mask;

        // Section 15.1 says that "Steps 2 and 4 [of the loop filter] are skipped... [if] there is
        // no DCT coefficient coded for the whole macroblock".
        nz_dc_mask == 0 && nz_ac_mask == 0
    }

    /// `Decoder.prepareYBR`: the `{abcdefghij}` border elements of the workspace.
    fn prepare_ybr(&mut self, mbx: usize, mby: usize) {
        if mbx == 0 {
            for y in 0..17 {
                self.ybr[y][7] = 0x81;
            }
            for y in 17..26 {
                self.ybr[y][7] = 0x81;
                self.ybr[y][23] = 0x81;
            }
        } else {
            for y in 0..17 {
                self.ybr[y][7] = self.ybr[y][7 + 16];
            }
            for y in 17..26 {
                self.ybr[y][7] = self.ybr[y][15];
                self.ybr[y][23] = self.ybr[y][31];
            }
        }
        if mby == 0 {
            for x in 7..28 {
                self.ybr[0][x] = 0x7f;
            }
            for x in 7..16 {
                self.ybr[17][x] = 0x7f;
            }
            for x in 23..32 {
                self.ybr[17][x] = 0x7f;
            }
        } else {
            let ys = self.img.y_stride;
            let cs = self.img.c_stride;
            for i in 0..16 {
                self.ybr[0][8 + i] = self.img.y[(16 * mby - 1) * ys + 16 * mbx + i];
            }
            for i in 0..8 {
                self.ybr[17][8 + i] = self.img.cb[(8 * mby - 1) * cs + 8 * mbx + i];
            }
            for i in 0..8 {
                self.ybr[17][24 + i] = self.img.cr[(8 * mby - 1) * cs + 8 * mbx + i];
            }
            if mbx == self.mbw - 1 {
                for i in 16..20 {
                    self.ybr[0][8 + i] = self.img.y[(16 * mby - 1) * ys + 16 * mbx + 15];
                }
            } else {
                for i in 16..20 {
                    self.ybr[0][8 + i] = self.img.y[(16 * mby - 1) * ys + 16 * mbx + i];
                }
            }
        }
        let mut y = 4;
        while y < 16 {
            for k in 0..4 {
                self.ybr[y][24 + k] = self.ybr[0][24 + k];
            }
            y += 4;
        }
    }

    /// `Decoder.reconstructMacroblock`: the predictor functions plus the inverse-DCT residuals.
    fn reconstruct_macroblock(&mut self, mbx: usize, mby: usize) {
        if self.use_pred_y16 {
            let p = check_top_left_pred(mbx, mby, self.pred_y16);
            self.pred_func16(p, 1, 8);
            for j in 0..4 {
                for i in 0..4 {
                    let n = 4 * j + i;
                    let y = 4 * j + 1;
                    let x = 4 * i + 8;
                    let mask = 1u32 << n;
                    if self.nz_ac_mask & mask != 0 {
                        self.inverse_dct4(y, x, 16 * n);
                    } else if self.nz_dc_mask & mask != 0 {
                        self.inverse_dct4_dc_only(y, x, 16 * n);
                    }
                }
            }
        } else {
            for j in 0..4 {
                for i in 0..4 {
                    let n = 4 * j + i;
                    let y = 4 * j + 1;
                    let x = 4 * i + 8;
                    self.pred_func4(self.pred_y4[j][i], y, x);
                    let mask = 1u32 << n;
                    if self.nz_ac_mask & mask != 0 {
                        self.inverse_dct4(y, x, 16 * n);
                    } else if self.nz_dc_mask & mask != 0 {
                        self.inverse_dct4_dc_only(y, x, 16 * n);
                    }
                }
            }
        }
        let p = check_top_left_pred(mbx, mby, self.pred_c8);
        self.pred_func8(p, YBR_BY, YBR_BX);
        if self.nz_ac_mask & 0x0f_0000 != 0 {
            self.inverse_dct8(YBR_BY, YBR_BX, B_COEFF_BASE);
        } else if self.nz_dc_mask & 0x0f_0000 != 0 {
            self.inverse_dct8_dc_only(YBR_BY, YBR_BX, B_COEFF_BASE);
        }
        self.pred_func8(p, YBR_RY, YBR_RX);
        if self.nz_ac_mask & 0xf0_0000 != 0 {
            self.inverse_dct8(YBR_RY, YBR_RX, R_COEFF_BASE);
        } else if self.nz_dc_mask & 0xf0_0000 != 0 {
            self.inverse_dct8_dc_only(YBR_RY, YBR_RX, R_COEFF_BASE);
        }
    }

    /// `Decoder.reconstruct`: one macroblock, returning whether inner loop filtering should be
    /// skipped for it.
    pub(super) fn reconstruct(&mut self, mbx: usize, mby: usize) -> bool {
        if self.segment_header.update_map {
            self.segment = if !self.fp.read_bit(self.segment_header.prob[0]) {
                self.fp.read_uint(self.segment_header.prob[1], 1) as usize
            } else {
                self.fp.read_uint(self.segment_header.prob[2], 1) as usize + 2
            };
        }
        let mut skip = false;
        if self.use_skip_prob {
            skip = self.fp.read_bit(self.skip_prob);
        }
        // Prepare the workspace.
        self.coeff = [0; 400];
        self.prepare_ybr(mbx, mby);
        // Parse the predictor modes.
        self.use_pred_y16 = self.fp.read_bit(145);
        if self.use_pred_y16 {
            self.parse_pred_mode_y16(mbx);
        } else {
            self.parse_pred_mode_y4(mbx);
        }
        self.parse_pred_mode_c8();
        // Parse the residuals.
        if !skip {
            skip = self.parse_residuals(mbx, mby, mby);
        } else {
            if self.use_pred_y16 {
                self.left_mb.nz_y16 = 0;
                self.up_mb[mbx].nz_y16 = 0;
            }
            self.left_mb.nz_mask = 0;
            self.up_mb[mbx].nz_mask = 0;
            self.nz_dc_mask = 0;
            self.nz_ac_mask = 0;
        }
        // Reconstruct the YCbCr data and copy it to the image.
        self.reconstruct_macroblock(mbx, mby);
        let ys = self.img.y_stride;
        let cs = self.img.c_stride;
        let mut i = (mby * ys + mbx) * 16;
        for y in 0..16 {
            self.img.y[i..i + 16].copy_from_slice(&self.ybr[YBR_YY + y][YBR_YX..YBR_YX + 16]);
            i += ys;
        }
        let mut i = (mby * cs + mbx) * 8;
        for y in 0..8 {
            self.img.cb[i..i + 8].copy_from_slice(&self.ybr[YBR_BY + y][YBR_BX..YBR_BX + 8]);
            self.img.cr[i..i + 8].copy_from_slice(&self.ybr[YBR_RY + y][YBR_RX..YBR_RX + 8]);
            i += cs;
        }
        skip
    }
}

/// Silence the unused-import warning for a type the struct definition uses.
const _: Option<Mb> = None;
/// `clip8` is re-exported through the IDCT; this keeps the dependency explicit.
const _: fn(i32) -> u8 = clip8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack_are_inverses() {
        for v in 0u8..16 {
            assert_eq!(pack(unpack(v), 0), u32::from(v));
        }
        assert_eq!(pack([1, 0, 1, 1], 4), 0b1101_0000);
        assert_eq!(unpack(0b1010), [0, 1, 0, 1]);
    }

    /// Go's `unpack` is a 16-entry table; this port computes the same four bits. The table's rows
    /// are little-endian, so entry 3 is `{1,1,0,0}` and not `{0,0,1,1}`.
    #[test]
    fn unpack_is_little_endian_like_gos_table() {
        assert_eq!(unpack(0), [0, 0, 0, 0]);
        assert_eq!(unpack(1), [1, 0, 0, 0]);
        assert_eq!(unpack(2), [0, 1, 0, 0]);
        assert_eq!(unpack(3), [1, 1, 0, 0]);
        assert_eq!(unpack(8), [0, 0, 0, 1]);
        assert_eq!(unpack(15), [1, 1, 1, 1]);
    }

    #[test]
    fn the_band_and_zigzag_tables_are_gos() {
        assert_eq!(BANDS, [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7, 0]);
        assert_eq!(
            ZIGZAG,
            [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15]
        );
        // The zigzag is a permutation of 0..16, and its first entry is the DC position.
        let mut seen = [false; 16];
        for &z in &ZIGZAG {
            assert!(!seen[z]);
            seen[z] = true;
        }
        assert_eq!(ZIGZAG[0], 0);
    }

    #[test]
    fn the_coefficient_bases_partition_the_workspace() {
        assert_eq!(B_COEFF_BASE, 256);
        assert_eq!(R_COEFF_BASE, 320);
        assert_eq!(WHT_COEFF_BASE, 384);
        assert_eq!(WHT_COEFF_BASE + 16, 400);
    }
}
