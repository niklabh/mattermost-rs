//! Port of the inverse DCT half of Go's `image/jpeg/dct.go` (go1.26.4): the fixed-point 1D IDCT
//! over rows then columns. Every operation is `int32` arithmetic in Go, which wraps; the port uses
//! wrapping operations so hostile coefficients produce Go's (wrapped) values rather than a panic.

/// Port of `block` (dct.go:93): 8×8 coefficients or samples.
pub(crate) type Block = [i32; 64];

// dct.go:134 — 60-bit fixed-point constants.
const COS1: u64 = 1130768441178740757;
const SIN1: u64 = 224923827593068887;
const COS3: u64 = 958619196450722178;
const SIN3: u64 = 640528868967736374;
const SQRT2INV: u64 = 815238614083298888;
const SQRT2INV_COS6: u64 = 311978311033955632;
const SQRT2INV_SIN6: u64 = 753182269664427492;

/// Port of `c` (dct.go:147): round a 60-bit constant to `bits` fractional bits.
const fn c(x: u64, bits: u32) -> i32 {
    ((x + (1 << (59 - bits))) >> (60 - bits)) as i32
}

/// Port of `dctBox` (dct.go:76).
#[inline(always)]
fn dct_box(x0: i32, x1: i32, kcos: i32, ksin: i32) -> (i32, i32) {
    let ksum = kcos.wrapping_mul(x0.wrapping_add(x1));
    let y0 = ksum.wrapping_add(ksin.wrapping_sub(kcos).wrapping_mul(x1));
    let y1 = ksum.wrapping_sub(kcos.wrapping_add(ksin).wrapping_mul(x0));
    (y0, y1)
}

/// Port of `idct` (dct.go:354). Inputs are dequantised coefficients; outputs are IDCT*8 samples
/// (Q10.3) centred on zero.
pub(crate) fn idct(b: &mut Block) {
    idct_rows(b);
    idct_cols(b);
}

/// Port of `idctRows` (dct.go:362).
fn idct_rows(b: &mut Block) {
    for i in 0..8 {
        let x = &mut b[8 * i..8 * i + 8];
        let mut x0 = x[0];
        let mut x7 = x[1];
        let mut x2 = x[2];
        let mut x5 = x[3];
        let mut x1 = x[4];
        let mut x6 = x[5];
        let mut x3 = x[6];
        let mut x4 = x[7];

        x0 = x0.wrapping_shl(17);
        x1 = x1.wrapping_shl(17);
        (x0, x1) = (x0.wrapping_add(x1), x0.wrapping_sub(x1));
        (x2, x3) = dct_box(x2, x3, c(SQRT2INV_COS6, 18), -c(SQRT2INV_SIN6, 18));
        (x1, x2) = (x1.wrapping_add(x2), x1.wrapping_sub(x2));
        (x0, x3) = (x0.wrapping_add(x3), x0.wrapping_sub(x3));

        x4 = x4.wrapping_shl(7);
        x7 = x7.wrapping_shl(7);
        (x7, x4) = (x7.wrapping_add(x4), x7.wrapping_sub(x4));

        x6 = x6.wrapping_mul(c(SQRT2INV, 8));
        x5 = x5.wrapping_mul(c(SQRT2INV, 8));

        (x7, x5) = (x7.wrapping_add(x5), x7.wrapping_sub(x5));
        (x4, x6) = (x4.wrapping_add(x6), x4.wrapping_sub(x6));

        (x4, x7) = dct_box(x4 >> 2, x7 >> 2, c(COS3, 12), -c(SIN3, 12));
        (x5, x6) = dct_box(x5 >> 2, x6 >> 2, c(COS1, 12), -c(SIN1, 12));

        (x0, x7) = (x0.wrapping_add(x7), x0.wrapping_sub(x7));
        (x1, x6) = (x1.wrapping_add(x6), x1.wrapping_sub(x6));
        (x2, x5) = (x2.wrapping_add(x5), x2.wrapping_sub(x5));
        (x3, x4) = (x3.wrapping_add(x4), x3.wrapping_sub(x4));

        x[0] = x0;
        x[1] = x1;
        x[2] = x2;
        x[3] = x3;
        x[4] = x4;
        x[5] = x5;
        x[6] = x6;
        x[7] = x7;
    }
}

/// Port of `idctCols` (dct.go:449).
fn idct_cols(b: &mut Block) {
    for i in 0..8 {
        let mut x0 = b[i];
        let mut x7 = b[8 + i];
        let mut x2 = b[2 * 8 + i];
        let mut x5 = b[3 * 8 + i];
        let mut x1 = b[4 * 8 + i];
        let mut x6 = b[5 * 8 + i];
        let mut x3 = b[6 * 8 + i];
        let mut x4 = b[7 * 8 + i];

        x0 = x0.wrapping_add(1 << 19);

        (x0, x1) = (x0.wrapping_add(x1) >> 2, x0.wrapping_sub(x1) >> 2);
        (x2, x3) = dct_box(
            x2 >> 13,
            x3 >> 13,
            c(SQRT2INV_COS6, 12),
            -c(SQRT2INV_SIN6, 12),
        );
        (x1, x2) = (x1.wrapping_add(x2), x1.wrapping_sub(x2));
        (x0, x3) = (x0.wrapping_add(x3), x0.wrapping_sub(x3));

        (x7, x4) = (x7.wrapping_add(x4), x7.wrapping_sub(x4));

        x5 = (x5 >> 13).wrapping_mul(c(SQRT2INV, 14));
        x6 = (x6 >> 13).wrapping_mul(c(SQRT2INV, 14));

        (x7, x5) = (x7.wrapping_add(x5), x7.wrapping_sub(x5));
        (x4, x6) = (x4.wrapping_add(x6), x4.wrapping_sub(x6));

        (x4, x7) = dct_box(x4 >> 14, x7 >> 14, c(COS3, 12), -c(SIN3, 12));
        (x5, x6) = dct_box(x5 >> 14, x6 >> 14, c(COS1, 12), -c(SIN1, 12));

        (x0, x7) = (x0.wrapping_add(x7), x0.wrapping_sub(x7));
        (x1, x6) = (x1.wrapping_add(x6), x1.wrapping_sub(x6));
        (x2, x5) = (x2.wrapping_add(x5), x2.wrapping_sub(x5));
        (x3, x4) = (x3.wrapping_add(x4), x3.wrapping_sub(x4));

        b[i] = x0 >> 18;
        b[8 + i] = x1 >> 18;
        b[2 * 8 + i] = x2 >> 18;
        b[3 * 8 + i] = x3 >> 18;
        b[4 * 8 + i] = x4 >> 18;
        b[5 * 8 + i] = x5 >> 18;
        b[6 * 8 + i] = x6 >> 18;
        b[7 * 8 + i] = x7 >> 18;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dc_only_block_is_flat() {
        // A DC coefficient of 8*v decodes to IDCT*8 = v*8 everywhere, i.e. v per sample after
        // the decoder's level shift reads the Q10.3 result... measured shape: every sample equal.
        let mut b = [0i32; 64];
        b[0] = 80;
        idct(&mut b);
        assert!(b.iter().all(|&v| v == b[0]), "{b:?}");
        assert_eq!(b[0], 10);
    }

    #[test]
    fn the_rounding_constants_match_go() {
        // c(sqrt2inv, 8) == 181 is quoted in dct.go's precision notes ("x[56] now UQ8.8 in [0, 181]").
        assert_eq!(c(SQRT2INV, 8), 181);
    }

    #[test]
    fn hostile_coefficients_wrap_instead_of_panicking() {
        let mut b = [i32::MAX; 64];
        idct(&mut b);
        let mut b = [i32::MIN; 64];
        idct(&mut b);
    }
}
