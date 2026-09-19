//! Port of the forward DCT half of Go's `image/jpeg/dct.go` (go1.26.4): an exact integer
//! fixed-point transform, so any deviation in a shift or a constant changes the coefficients the
//! encoder quantizes. Go's `int32` arithmetic wraps, so every operation here is `wrapping_*`.

/// `blockSize` (dct.go:95).
pub const BLOCK_SIZE: usize = 64;

/// Port of `block` (dct.go:93): an 8×8 block in natural order.
pub type Block = [i32; BLOCK_SIZE];

// dct.go:134-145 — 60-bit fixed-point constants.
const COS1: u64 = 1130768441178740757;
const SIN1: u64 = 224923827593068887;
const COS3: u64 = 958619196450722178;
const SIN3: u64 = 640528868967736374;
const SQRT2: u64 = 1630477228166597777;
const SQRT2_COS6: u64 = 623956622067911264;
const SQRT2_SIN6: u64 = 1506364539328854985;

/// Port of `c` (dct.go:147): the constant `x` rounded to `bits` fractional bits.
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

#[inline(always)]
fn bf(a: i32, b: i32) -> (i32, i32) {
    (a.wrapping_add(b), a.wrapping_sub(b))
}

/// Port of `fdct` (dct.go:153). Inputs UQ8.0; outputs Q13.0.
pub fn fdct(b: &mut Block) {
    fdct_cols(b);
    fdct_rows(b);
}

/// Port of `fdctCols` (dct.go:161).
fn fdct_cols(b: &mut Block) {
    for i in 0..8 {
        let (x0, x7) = bf(b[i], b[7 * 8 + i]);
        let (x1, x6) = bf(b[8 + i], b[6 * 8 + i]);
        let (x2, x5) = bf(b[2 * 8 + i], b[5 * 8 + i]);
        let (x3, x4) = bf(b[3 * 8 + i], b[4 * 8 + i]);

        let (x4, x7) = dct_box(x4, x7, c(COS3, 18), c(SIN3, 18));
        let (x5, x6) = dct_box(x5, x6, c(COS1, 18), c(SIN1, 18));
        let (x0, x3) = bf(x0, x3);
        let (x1, x2) = bf(x1, x2);

        let (x2, x3) = dct_box(x2, x3, c(SQRT2_COS6, 18), c(SQRT2_SIN6, 18));
        let (x0, x1) = bf(x0, x1);

        b[i] = x0.wrapping_sub(128 * 8).wrapping_shl(18);
        b[4 * 8 + i] = x1.wrapping_shl(18);
        b[2 * 8 + i] = x2;
        b[6 * 8 + i] = x3;

        let (x4, x6) = bf(x4, x6);
        let (x7, x5) = bf(x7, x5);

        let x5 = (x5 >> 12).wrapping_mul(c(SQRT2, 12));
        let x6 = (x6 >> 12).wrapping_mul(c(SQRT2, 12));
        let (x7, x4) = bf(x7, x4);

        b[8 + i] = x7;
        b[3 * 8 + i] = x5;
        b[5 * 8 + i] = x6;
        b[7 * 8 + i] = x4;
    }
}

/// Port of `fdctRows` (dct.go:243).
fn fdct_rows(b: &mut Block) {
    for i in 0..8 {
        let x = &mut b[8 * i..8 * i + 8];
        let (x0, x7) = bf(x[0], x[7]);
        let (x1, x6) = bf(x[1], x[6]);
        let (x2, x5) = bf(x[2], x[5]);
        let (x3, x4) = bf(x[3], x[4]);

        let (x4, x7) = dct_box(x4 >> 14, x7 >> 14, c(COS3, 14), c(SIN3, 14));
        let (x5, x6) = dct_box(x5 >> 14, x6 >> 14, c(COS1, 14), c(SIN1, 14));
        let (x0, x3) = bf(x0, x3);
        let (x1, x2) = bf(x1, x2);

        let (x2, x3) = dct_box(x2 >> 14, x3 >> 14, c(SQRT2_COS6, 14), c(SQRT2_SIN6, 14));
        let (x0, x1) = bf(x0, x1);
        let (x4, x6) = bf(x4, x6);
        let (x7, x5) = bf(x7, x5);

        let x5 = (x5 >> 14).wrapping_mul(c(SQRT2, 14));
        let x6 = (x6 >> 14).wrapping_mul(c(SQRT2, 14));
        let (x7, x4) = bf(x7, x4);

        let cut = |v: i32| v.wrapping_add(1 << 17) >> 18;
        x[0] = cut(x0);
        x[1] = cut(x7);
        x[2] = cut(x2);
        x[3] = cut(x5);
        x[4] = cut(x1);
        x[5] = cut(x6);
        x[6] = cut(x3);
        x[7] = cut(x4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_block_has_only_dc() {
        let mut b = [200; BLOCK_SIZE];
        fdct(&mut b);
        // DC of a flat 200 block: (200-128)*64 = 4608 in Q13.0 (8× the orthonormal value).
        assert_eq!(b[0], (200 - 128) * 64);
        assert!(b[1..].iter().all(|&v| v == 0), "{b:?}");
    }

    #[test]
    fn constants_round_like_go() {
        // c(sqrt2, 12) = round(sqrt(2) * 4096) = 5793.
        assert_eq!(c(SQRT2, 12), 5793);
        assert_eq!(c(COS1, 18), 257107);
    }
}
