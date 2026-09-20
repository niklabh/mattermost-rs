//! Port of `golang.org/x/image/vp8/idct.go`: the inverse DCT and the inverse Walsh-Hadamard
//! transform (sections 14.3 and 14.4).
//!
//! Two things about the arithmetic. Every `>>` is on a signed `int32` and is therefore an
//! arithmetic shift in both languages, which matters: `(a+d)>>3` on a negative residual rounds
//! towards minus infinity, not towards zero, and `/8` would not. And every multiply and add is
//! `wrapping_*`, because Go's `int32` wraps and this one really can: a coefficient is an `int16`
//! that `parseResiduals4` truncated from a much larger product, and `32767 * 85627` is already
//! past `int32`'s range. A corrupt stream reaches that, and Go's answer for it is the wrapped
//! one.

use super::Decoder;

/// `clip8`.
pub(super) fn clip8(i: i32) -> u8 {
    if i < 0 {
        return 0;
    }
    if i > 255 {
        return 255;
    }
    i as u8
}

/// 65536 * cos(pi/8) * sqrt(2).
const C1: i32 = 85627;
/// 65536 * sin(pi/8) * sqrt(2).
const C2: i32 = 35468;

impl Decoder<'_> {
    /// `Decoder.inverseDCT4`.
    pub(super) fn inverse_dct4(&mut self, y: usize, x: usize, coeff_base: usize) {
        let mut m = [[0i32; 4]; 4];
        for (cb, row) in (coeff_base..).zip(m.iter_mut()) {
            let a = i32::from(self.coeff[cb]).wrapping_add(i32::from(self.coeff[cb + 8]));
            let b = i32::from(self.coeff[cb]).wrapping_sub(i32::from(self.coeff[cb + 8]));
            let c = (i32::from(self.coeff[cb + 4]).wrapping_mul(C2) >> 16)
                .wrapping_sub(i32::from(self.coeff[cb + 12]).wrapping_mul(C1) >> 16);
            let d = (i32::from(self.coeff[cb + 4]).wrapping_mul(C1) >> 16)
                .wrapping_add(i32::from(self.coeff[cb + 12]).wrapping_mul(C2) >> 16);
            row[0] = a.wrapping_add(d);
            row[1] = b.wrapping_add(c);
            row[2] = b.wrapping_sub(c);
            row[3] = a.wrapping_sub(d);
        }
        #[allow(clippy::needless_range_loop)]
        for j in 0..4 {
            let dc = m[0][j].wrapping_add(4);
            let a = dc.wrapping_add(m[2][j]);
            let b = dc.wrapping_sub(m[2][j]);
            let c = (m[1][j].wrapping_mul(C2) >> 16).wrapping_sub(m[3][j].wrapping_mul(C1) >> 16);
            let d = (m[1][j].wrapping_mul(C1) >> 16).wrapping_add(m[3][j].wrapping_mul(C2) >> 16);
            let row = &mut self.ybr[y + j];
            row[x] = clip8(i32::from(row[x]).wrapping_add(a.wrapping_add(d) >> 3));
            row[x + 1] = clip8(i32::from(row[x + 1]).wrapping_add(b.wrapping_add(c) >> 3));
            row[x + 2] = clip8(i32::from(row[x + 2]).wrapping_add(b.wrapping_sub(c) >> 3));
            row[x + 3] = clip8(i32::from(row[x + 3]).wrapping_add(a.wrapping_sub(d) >> 3));
        }
    }

    /// `Decoder.inverseDCT4DCOnly`.
    pub(super) fn inverse_dct4_dc_only(&mut self, y: usize, x: usize, coeff_base: usize) {
        let dc = (i32::from(self.coeff[coeff_base]) + 4) >> 3;
        for j in 0..4 {
            for i in 0..4 {
                self.ybr[y + j][x + i] = clip8(i32::from(self.ybr[y + j][x + i]) + dc);
            }
        }
    }

    /// `Decoder.inverseDCT8`.
    pub(super) fn inverse_dct8(&mut self, y: usize, x: usize, coeff_base: usize) {
        self.inverse_dct4(y, x, coeff_base);
        self.inverse_dct4(y, x + 4, coeff_base + 16);
        self.inverse_dct4(y + 4, x, coeff_base + 32);
        self.inverse_dct4(y + 4, x + 4, coeff_base + 48);
    }

    /// `Decoder.inverseDCT8DCOnly`.
    pub(super) fn inverse_dct8_dc_only(&mut self, y: usize, x: usize, coeff_base: usize) {
        self.inverse_dct4_dc_only(y, x, coeff_base);
        self.inverse_dct4_dc_only(y, x + 4, coeff_base + 16);
        self.inverse_dct4_dc_only(y + 4, x, coeff_base + 32);
        self.inverse_dct4_dc_only(y + 4, x + 4, coeff_base + 48);
    }

    /// `Decoder.inverseWHT16`: the WHT coefficients at 384 become the DC coefficient of each of the
    /// sixteen 4x4 luma blocks.
    pub(super) fn inverse_wht16(&mut self) {
        let mut m = [0i32; 16];
        for i in 0..4 {
            let a0 = i32::from(self.coeff[384 + i]) + i32::from(self.coeff[384 + 12 + i]);
            let a1 = i32::from(self.coeff[384 + 4 + i]) + i32::from(self.coeff[384 + 8 + i]);
            let a2 = i32::from(self.coeff[384 + 4 + i]) - i32::from(self.coeff[384 + 8 + i]);
            let a3 = i32::from(self.coeff[384 + i]) - i32::from(self.coeff[384 + 12 + i]);
            m[i] = a0 + a1;
            m[8 + i] = a0 - a1;
            m[4 + i] = a3 + a2;
            m[12 + i] = a3 - a2;
        }
        let mut out = 0usize;
        for i in 0..4 {
            let dc = m[i * 4] + 3;
            let a0 = dc + m[3 + i * 4];
            let a1 = m[1 + i * 4] + m[2 + i * 4];
            let a2 = m[1 + i * 4] - m[2 + i * 4];
            let a3 = dc - m[3 + i * 4];
            self.coeff[out] = ((a0 + a1) >> 3) as i16;
            self.coeff[out + 16] = ((a3 + a2) >> 3) as i16;
            self.coeff[out + 32] = ((a0 - a1) >> 3) as i16;
            self.coeff[out + 48] = ((a3 - a2) >> 3) as i16;
            out += 64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip8_saturates() {
        assert_eq!(clip8(-1), 0);
        assert_eq!(clip8(0), 0);
        assert_eq!(clip8(255), 255);
        assert_eq!(clip8(256), 255);
        assert_eq!(clip8(i32::MIN), 0);
        assert_eq!(clip8(i32::MAX), 255);
    }

    /// `>>` on a negative `int32` floors; `/` would truncate towards zero. The IDCT's `(x+4)>>3`
    /// and the WHT's `(x+3)>>3` both rely on flooring.
    #[test]
    fn the_shifts_floor_rather_than_truncate() {
        assert_eq!((-1i32) >> 3, -1);
        assert_eq!((-1i32) / 8, 0);
        assert_eq!((-8i32) >> 3, -1);
        assert_eq!((-9i32) >> 3, -2);
        // The C2 multiply is also an arithmetic shift of a negative product.
        assert_eq!((-1i32).wrapping_mul(C2) >> 16, -1);
        assert_eq!((-1i32).wrapping_mul(C1) >> 16, -2);
        assert_eq!(1i32.wrapping_mul(C1) >> 16, 1);
        // And it wraps where Go's int32 wraps: a coefficient of -32768 times C1 is past the range.
        assert_eq!((-32768i32).wrapping_mul(C1), (-32768i64 * 85627) as i32);
    }
}
