//! Port of Go's `math.Sin` (math/sin.go:192, math/trig_reduce.go) as the arm64 compiler emits it.
//!
//! `math.Sin` is pure Go on arm64 (`haveArchSin` is false outside s390x, math/stubs.go:138), and
//! the compiler fuses its Cody–Waite reduction and both polynomials into FMADDD/FMSUBD chains.
//! `f64::sin` (libm) is a different algorithm and differs in the last bit for some arguments —
//! which is enough to move a Lanczos weight and, through it, a stored pixel. Every fused site
//! below cites the instruction `go tool objdump` shows for go1.26.4/arm64.

use crate::fma::{madd, msub};

/// Pi/4 split into three parts (sin.go:194-196), as the bit patterns Go documents beside them.
const PI4A: f64 = f64::from_bits(0x3fe9_21fb_4000_0000);
const PI4B: f64 = f64::from_bits(0x3e64_442d_0000_0000);
const PI4C: f64 = f64::from_bits(0x3ce8_4698_98cc_5170);

/// `4 / Pi`, which Go folds as an exact constant and rounds once (0x3ff45f306dc9c883, measured).
/// `4.0 / std::f64::consts::PI` rounds twice.
const FOUR_OVER_PI: f64 = f64::from_bits(0x3ff4_5f30_6dc9_c883);

/// `reduceThreshold` (trig_reduce.go:14).
const REDUCE_THRESHOLD: f64 = (1u64 << 29) as f64;

/// `_sin` (sin.go:92), by the bit patterns in its comments.
const SIN: [f64; 6] = [
    f64::from_bits(0x3de5_d8fd_1fd1_9ccd),
    f64::from_bits(0xbe5a_e5e5_a929_1f5d),
    f64::from_bits(0x3ec7_1de3_567d_48a1),
    f64::from_bits(0xbf2a_01a0_19bf_df03),
    f64::from_bits(0x3f81_1111_1110_f7d0),
    f64::from_bits(0xbfc5_5555_5555_5548),
];

/// `_cos` (sin.go:102), by the bit patterns in its comments.
const COS: [f64; 6] = [
    f64::from_bits(0xbda8_fa49_a086_1a9b),
    f64::from_bits(0x3e21_ee9d_7b4e_3f05),
    f64::from_bits(0xbe92_7e4f_7eac_4bc6),
    f64::from_bits(0x3efa_01a0_19c8_44f5),
    f64::from_bits(0xbf56_c16c_16c1_4f91),
    f64::from_bits(0x3fa5_5555_5555_554b),
];

/// `(((((c[0]*zz)+c[1])*zz+c[2])*zz+c[3])*zz+c[4])*zz+c[5]` — every step one FMADDD on arm64
/// (sin.go:236/238: five consecutive `FMADDD F4, F5, F2, F4`-shaped instructions).
fn poly(c: &[f64; 6], zz: f64) -> f64 {
    let mut p = madd(c[1], c[0], zz);
    p = madd(c[2], p, zz);
    p = madd(c[3], p, zz);
    p = madd(c[4], p, zz);
    madd(c[5], p, zz)
}

/// Port of `math.Sin` (math/sin.go:185).
pub fn sin(x: f64) -> f64 {
    if x == 0.0 || x.is_nan() {
        return x;
    }
    if x.is_infinite() {
        return f64::NAN;
    }
    let mut sign = false;
    let mut x = x;
    if x < 0.0 {
        x = -x;
        sign = true;
    }
    let (mut j, z) = if x >= REDUCE_THRESHOLD {
        trig_reduce(x)
    } else {
        // sin.go:218: FMULD then FCVTZUD — not fused.
        let mut j = (x * FOUR_OVER_PI) as u64;
        let mut y = j as f64;
        if j & 1 == 1 {
            j += 1;
            y += 1.0;
        }
        j &= 7;
        // sin.go:227: `((x - y*PI4A) - y*PI4B) - y*PI4C` is three FMSUBD.
        (j, msub(msub(msub(x, y, PI4A), y, PI4B), y, PI4C))
    };
    if j > 3 {
        sign = !sign;
        j -= 4;
    }
    let zz = z * z;
    let y = if j == 1 || j == 2 {
        // sin.go:236: `FMSUBD` for `1.0 - 0.5*zz`, FMULD for zz*zz, then FMADDD adding the
        // product of zz*zz and the polynomial.
        madd(msub(1.0, 0.5, zz), zz * zz, poly(&COS, zz))
    } else {
        // sin.go:238: FMULD for z*zz, then `FMADDD F2, F1, F3, F1` = z + (z*zz)*poly.
        madd(z, z * zz, poly(&SIN, zz))
    };
    if sign { -y } else { y }
}

/// `mPi4` (trig_reduce.go:77): the binary digits of 4/π.
const M_PI4: [u64; 20] = [
    0x0000000000000001,
    0x45f306dc9c882a53,
    0xf84eafa3ea69bb81,
    0xb6c52b3278872083,
    0xfca2c757bd778ac3,
    0x6e48dc74849ba5c0,
    0x0c925dd413a32439,
    0xfc3bd63962534e7d,
    0xd1046bea5d768909,
    0xd338e04d68befc82,
    0x7323ac7306a673e9,
    0x3908bf177bf25076,
    0x3ff12fffbc0b301f,
    0xde5e2316b414da3e,
    0xda6cfd9e4f96136e,
    0x9e8c7ecd3cbfd45a,
    0xea4f758fd7cbe2f6,
    0x7a0e73ef14a525d4,
    0xd7f6bf623f1aba10,
    0xac06608df8f6d757,
];

/// Go's `x >> s`, which is 0 for `s >= 64` where Rust's `>>` would overflow.
fn shr(x: u64, s: u32) -> u64 {
    x.checked_shr(s).unwrap_or(0)
}

/// Port of `trigReduce` (math/trig_reduce.go:27), Payne–Hanek reduction for `x >= 2^29`.
/// Unreachable from the Lanczos kernel (its arguments are below 3π) but part of `Sin`.
fn trig_reduce(x: f64) -> (u64, f64) {
    const PI4: f64 = std::f64::consts::FRAC_PI_4;
    const SHIFT: u64 = 52;
    const MASK: u64 = 0x7ff;
    const BIAS: i64 = 1023;
    if x < PI4 {
        return (0, x);
    }
    let mut ix = x.to_bits();
    let exp = ((ix >> SHIFT) & MASK) as i64 - BIAS - SHIFT as i64;
    ix &= !(MASK << SHIFT);
    ix |= 1 << SHIFT;
    let digit = ((exp + 61) / 64) as usize;
    let bitshift = ((exp + 61) % 64) as u32;
    let d = |i: usize| M_PI4.get(digit + i).copied().unwrap_or(0);
    let z0 = (d(0) << bitshift) | shr(d(1), 64 - bitshift);
    let z1 = (d(1) << bitshift) | shr(d(2), 64 - bitshift);
    let z2 = (d(2) << bitshift) | shr(d(3), 64 - bitshift);
    let z2hi = ((u128::from(z2) * u128::from(ix)) >> 64) as u64;
    let z1p = u128::from(z1) * u128::from(ix);
    let (z1hi, z1lo) = ((z1p >> 64) as u64, z1p as u64);
    let z0lo = z0.wrapping_mul(ix);
    let (lo, c) = z1lo.overflowing_add(z2hi);
    let hi = z0lo.wrapping_add(z1hi).wrapping_add(u64::from(c));
    let mut j = hi >> 61;
    let mut hi = hi << 3 | lo >> 61;
    let lz = hi.leading_zeros();
    let e = (BIAS as u64).wrapping_sub(u64::from(lz) + 1);
    hi = (hi << (lz + 1)) | shr(lo, 64 - (lz + 1));
    hi >>= 64 - SHIFT;
    hi |= e << SHIFT;
    let mut z = f64::from_bits(hi);
    if j & 1 == 1 {
        j += 1;
        j &= 7;
        z -= 1.0;
    }
    (j, z * PI4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `math.Sin` at every point the oracle recorded (`behaviour_imaging_resize.json` → `sin`),
    /// bit for bit: the Lanczos range, reduction boundaries, huge arguments, signed zero,
    /// infinities, NaN, and one argument that separates the fused reduction from the unfused one.
    #[test]
    fn matches_go_bit_for_bit() {
        let points = crate::testsupport::fixture("resize")["sin"]["points"]
            .as_array()
            .unwrap();
        for p in points {
            let x = f64::from_bits(u64::from_str_radix(p[0].as_str().unwrap(), 16).unwrap());
            let want = u64::from_str_radix(p[1].as_str().unwrap(), 16).unwrap();
            let got = sin(x);
            if f64::from_bits(want).is_nan() {
                assert!(got.is_nan(), "{x}");
            } else {
                assert_eq!(got.to_bits(), want, "sin({x:e})");
            }
        }
        assert!(points.len() > 140);
    }

    /// Dense runs hashed by the oracle (SHA-256 of each result's little-endian bits): the sparse
    /// points cannot tell a single unfused polynomial or reduction step from the fused one; these
    /// can. Each generator mirrors the oracle's Go expression, fused where Go fuses it.
    #[test]
    fn dense_sequences_match_go() {
        let want = &crate::testsupport::fixture("resize")["sin"]["dense"];
        type Seq = (&'static str, usize, fn(usize) -> f64);
        let seqs: [Seq; 5] = [
            ("linear", 400_000, |i| i as f64 * 0.0000712345),
            // `math.Pi * (float64(i)/200000*6 - 3)`: the subtraction is an FNMSUBD.
            ("lanczos", 200_000, |i| {
                std::f64::consts::PI * crate::fma::nmsub(i as f64 / 200_000.0, 6.0, 3.0)
            }),
            ("neg", 100_000, |i| -(i as f64) * 0.000311),
            // `536870912.0 + float64(i)*977.123`: an FMADDD.
            ("large", 20_000, |i| {
                crate::fma::madd(536_870_912.0, i as f64, 977.123)
            }),
            ("huge", 20_000, |i| {
                (1.0 + i as f64 / 20_000.0) * 2f64.powi(30 + (i % 900) as i32)
            }),
        ];
        for (name, n, f) in seqs {
            let mut bytes = Vec::with_capacity(n * 8);
            for i in 0..n {
                bytes.extend_from_slice(&sin(f(i)).to_bits().to_le_bytes());
            }
            assert_eq!(crate::testsupport::sha(&bytes), want[name], "{name}");
        }
    }
}
