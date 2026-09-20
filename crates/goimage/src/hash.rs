//! `hash/crc32` (IEEE) and `hash/adler32`. Both are fully specified checksums, so any correct
//! implementation matches Go's; these are the plain table and modulo forms.

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// A running `crc32.NewIEEE()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(0)
    }

    /// `Write`.
    pub fn update(&mut self, p: &[u8]) {
        let mut c = !self.0;
        for &b in p {
            c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = !c;
    }

    /// `Sum32`.
    pub fn sum(&self) -> u32 {
        self.0
    }
}

/// `crc32.ChecksumIEEE`.
pub fn crc32_ieee(p: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(p);
    c.sum()
}

/// A running `adler32.New()`.
#[derive(Clone, Copy, Debug)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Adler32 { a: 1, b: 0 }
    }
}

impl Adler32 {
    pub fn new() -> Adler32 {
        Adler32::default()
    }

    /// `Write`.
    pub fn update(&mut self, p: &[u8]) {
        const MOD: u32 = 65521;
        // 5552 is the largest n with 255n(n+1)/2 + (n+1)(MOD-1) <= 2^32-1 (adler32.go's nmax).
        for chunk in p.chunks(5552) {
            for &x in chunk {
                self.a += u32::from(x);
                self.b += self.a;
            }
            self.a %= MOD;
            self.b %= MOD;
        }
    }

    /// `Sum32`.
    pub fn sum(&self) -> u32 {
        self.b << 16 | self.a
    }
}

/// `adler32.Checksum`.
pub fn adler32(p: &[u8]) -> u32 {
    let mut a = Adler32::new();
    a.update(p);
    a.sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // Go: crc32.ChecksumIEEE([]byte("IEND")) and adler32.Checksum([]byte("Wikipedia")).
        assert_eq!(crc32_ieee(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(adler32(b""), 1);
        let big = vec![0xffu8; 100_000];
        let mut split = Adler32::new();
        split.update(&big[..7]);
        split.update(&big[7..]);
        assert_eq!(split.sum(), adler32(&big));
    }
}
