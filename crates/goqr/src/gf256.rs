//! Port of `gf256` (gf256/gf256.go): arithmetic in GF(2⁸) and the Reed–Solomon encoder QR codes
//! use. Only what `qr/coding` calls is here — the field with polynomial `0x11d` and generator 2,
//! and `RSEncoder.ECC`.

use std::sync::LazyLock;

/// Port of `gf256.Field`: the log and exp tables of one field.
pub(crate) struct Field {
    log: [u8; 256],
    exp: [u8; 510],
}

/// `mul` (gf256.go): carry-less multiplication reduced by `poly`.
fn mul(mut x: u32, mut y: u32, poly: u32) -> u32 {
    let mut z = 0;
    while x > 0 {
        if x & 1 != 0 {
            z ^= y;
        }
        x >>= 1;
        y <<= 1;
        if y & 0x100 != 0 {
            y ^= poly;
        }
    }
    z
}

impl Field {
    /// Port of `NewField`, without the validation panics: the one field QR codes use is fixed and
    /// valid, and nothing else calls this.
    fn new(poly: u32, alpha: u32) -> Self {
        let mut field = Field {
            log: [0; 256],
            exp: [0; 510],
        };
        let mut x: u32 = 1;
        for i in 0..255 {
            // Truncated to a byte as Go's `byte(x)` does; `x` never exceeds 255 for a field poly.
            let b = (x & 0xff) as u8;
            field.exp[i] = b;
            field.exp[i + 255] = b;
            field.log[x as usize & 0xff] = i as u8;
            x = mul(x, alpha, poly);
        }
        field.log[0] = 255;
        field
    }

    /// `Field.Mul`.
    pub(crate) fn mul(&self, x: u8, y: u8) -> u8 {
        if x == 0 || y == 0 {
            return 0;
        }
        self.exp[self.log[x as usize] as usize + self.log[y as usize] as usize]
    }

    /// `Field.Exp`.
    pub(crate) fn exp(&self, e: i32) -> u8 {
        if e < 0 {
            return 0;
        }
        self.exp[(e % 255) as usize]
    }

    /// `Field.Log`: `-1` for zero.
    pub(crate) fn log(&self, x: u8) -> i32 {
        if x == 0 {
            return -1;
        }
        i32::from(self.log[x as usize])
    }
}

/// `coding.Field`: `gf256.NewField(0x11d, 2)`.
pub(crate) static FIELD: LazyLock<Field> = LazyLock::new(|| Field::new(0x11d, 2));

/// Port of `gf256.RSEncoder`.
pub(crate) struct RsEncoder {
    c: usize,
    /// The generator's coefficients as logs, 255 standing for log 0.
    lgen: Vec<u8>,
}

impl RsEncoder {
    /// Port of `NewRSEncoder`, with `Field.gen` inlined.
    pub(crate) fn new(c: usize) -> Self {
        let f = &*FIELD;
        let mut p = vec![0u8; c + 1];
        p[c] = 1;
        for i in 0..c {
            let coefficient = f.exp(i as i32);
            for j in 0..c {
                p[j] = f.mul(p[j], coefficient) ^ p[j + 1];
            }
            p[c] = f.mul(p[c], coefficient);
        }
        let lgen = p
            .iter()
            .map(|&x| if x == 0 { 255 } else { f.log(x) as u8 })
            .collect();
        RsEncoder { c, lgen }
    }

    /// Port of `RSEncoder.ECC`: the `c` check bytes for `data`.
    pub(crate) fn ecc(&self, data: &[u8]) -> Vec<u8> {
        if self.c == 0 {
            return Vec::new();
        }
        let f = &*FIELD;
        let mut p = vec![0u8; data.len() + self.c];
        p[..data.len()].copy_from_slice(data);
        let lgen = &self.lgen[1..];
        for i in 0..data.len() {
            let c = p[i];
            if c == 0 {
                continue;
            }
            let base = f.log[c as usize] as usize;
            for (j, &lg) in lgen.iter().enumerate() {
                if lg != 255 {
                    p[i + 1 + j] ^= f.exp[base + lg as usize];
                }
            }
        }
        p[data.len()..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_is_a_field() {
        let f = &*FIELD;
        for x in 1..=255u8 {
            assert_eq!(f.exp(f.log(x)), x);
        }
        assert_eq!(f.mul(2, 128), 0x1d, "x·x⁷ reduces by 0x11d");
        assert_eq!(f.exp(-1), 0);
        assert_eq!(f.log(0), -1);
    }

    /// The worked example every QR reference uses: version 1-M "01234567".
    #[test]
    fn check_bytes_of_the_reference_block() {
        let data = [
            0x10, 0x20, 0x0c, 0x56, 0x61, 0x80, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11,
            0xec, 0x11,
        ];
        assert_eq!(
            RsEncoder::new(10).ecc(&data),
            [0xa5, 0x24, 0xd4, 0xc1, 0xed, 0x36, 0xc7, 0x87, 0x2c, 0x55]
        );
    }
}
