//! Port of `qr/png.go`: the QR-specific PNG writer — 1-bit greyscale, a four-module border, one
//! fixed-Huffman deflate block that repeats each row `scale` times by back-reference, and an
//! Adler-32 computed in closed form.

use crate::Code;

const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n";
const COMMENT: &[u8] = b"Software\x00QR-PNG http://qr.swtch.com/";

/// `pngWriter.encode`.
pub(crate) fn encode(c: &Code) -> Vec<u8> {
    let scale = c.scale;
    let siz = c.size;
    let mut buf = Vec::new();
    buf.extend_from_slice(PNG_HEADER);

    let side = ((siz + 8) * scale) as u32;
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&[1, 0, 0, 0, 0]);
    write_chunk(&mut buf, b"IHDR", &ihdr);
    write_chunk(&mut buf, b"tEXt", COMMENT);

    let mut z = BitWriter::default();
    z.write_code(c);
    write_chunk(&mut buf, b"IDAT", &z.bytes);
    write_chunk(&mut buf, b"IEND", &[]);
    buf
}

/// `pngWriter.writeChunk`: length, name, data, CRC-32 of name and data.
fn write_chunk(buf: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(data);
    let mut crc = Crc32::default();
    crc.update(name);
    crc.update(data);
    buf.extend_from_slice(&crc.finish().to_be_bytes());
}

/// IEEE CRC-32, as `hash/crc32.NewIEEE`.
#[derive(Default)]
struct Crc32 {
    state: u32,
}

impl Crc32 {
    fn update(&mut self, data: &[u8]) {
        let mut crc = !self.state;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        self.state = !crc;
    }

    fn finish(&self) -> u32 {
        self.state
    }
}

const AMOD: u32 = 65521;

/// `aupdate`: `n` bytes of value `pi` into an Adler-32 state, in closed form — with Go's `uint32`
/// wrap-around in `m * (m + 1) / 2`, kept.
fn aupdate(a: u32, b: u32, pi: u8, n: usize) -> (u32, u32) {
    if pi == 0 {
        let b = (b.wrapping_add(((n as u32) % AMOD).wrapping_mul(a))) % AMOD;
        return (a, b);
    }
    let m = n as u32;
    let mut b = b.wrapping_add((m % AMOD).wrapping_mul(a));
    b %= AMOD;
    b = b.wrapping_add((m.wrapping_mul(m.wrapping_add(1)) / 2 % AMOD).wrapping_mul(u32::from(pi)));
    b %= AMOD;
    let mut a = a.wrapping_add((m % AMOD).wrapping_mul(u32::from(pi)));
    a %= AMOD;
    (a, b)
}

/// `adigest`.
struct Adler {
    a: u32,
    b: u32,
}

impl Default for Adler {
    fn default() -> Self {
        Adler { a: 1, b: 0 }
    }
}

impl Adler {
    fn write_n(&mut self, p: &[u8], n: usize) {
        for _ in 0..n {
            for &pi in p {
                (self.a, self.b) = aupdate(self.a, self.b, pi, 1);
            }
        }
    }

    fn write_n_byte(&mut self, pi: u8, n: usize) {
        (self.a, self.b) = aupdate(self.a, self.b, pi, n);
    }

    fn sum32(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

/// `bitWriter`: a deflate bit stream, least significant bit first.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit: u32,
    nbit: u32,
    adler32: Adler,
}

impl BitWriter {
    /// `writeCode`.
    fn write_code(&mut self, c: &Code) {
        const FT_NONE: u8 = 0;
        self.adler32 = Adler::default();
        self.bytes.clear();
        self.nbit = 0;
        let scale = c.scale;
        let siz = c.size;

        // The zlib header: 0x78 and the check byte that makes it a multiple of 31.
        let cmf: u16 = 0x78;
        let flg = (31 - ((cmf << 8) % 31)) as u8;
        self.bytes.extend_from_slice(&[0x78, flg]);

        self.write_bits(1, 1, false); // final block
        self.write_bits(1, 2, false); // fixed Huffman

        let n = (scale * (siz + 8)).div_ceil(8);
        self.white_border(n, scale);

        let mut row = vec![0u8; 1 + n];
        for y in 0..siz as isize {
            row[0] = FT_NONE;
            let mut j = 1;
            let mut z: u8 = 0;
            let mut nz = 0;
            for x in -4..siz as isize + 4 {
                for _ in 0..scale {
                    z <<= 1;
                    if !c.black(x, y) {
                        z |= 1;
                    }
                    nz += 1;
                    if nz == 8 {
                        row[j] = z;
                        j += 1;
                        nz = 0;
                    }
                }
            }
            if j < row.len() {
                row[j] = z;
            }
            for &z in &row {
                self.byte(z);
            }
            self.repeat((scale - 1) * (1 + n), 1 + n);
            self.adler32.write_n(&row, scale);
        }

        self.white_border(n, scale);

        self.hcode(256);
        self.flush_bits();
        let sum = self.adler32.sum32();
        self.bytes.extend_from_slice(&sum.to_be_bytes());
    }

    /// The `4 * scale` white rows above and below the symbol.
    fn white_border(&mut self, n: usize, scale: usize) {
        self.byte(0);
        self.byte(255);
        self.repeat(n - 1, 1);
        self.repeat((4 * scale - 1) * (1 + n), 1 + n);
        for _ in 0..4 * scale {
            self.adler32.write_n_byte(0, 1);
            self.adler32.write_n_byte(255, n);
        }
    }

    /// `writeBits`: `nbit` bits of `bit`, bit-reversed first for a Huffman code.
    fn write_bits(&mut self, mut bit: u32, nbit: u32, rev: bool) {
        if rev {
            let mut br = 0;
            for i in 0..nbit {
                br |= ((bit >> i) & 1) << (nbit - 1 - i);
            }
            bit = br;
        }
        self.bit |= bit << self.nbit;
        self.nbit += nbit;
        while self.nbit >= 8 {
            self.bytes.push(self.bit as u8);
            self.bit >>= 8;
            self.nbit -= 8;
        }
    }

    fn flush_bits(&mut self) {
        if self.nbit > 0 {
            self.bytes.push(self.bit as u8);
            self.nbit = 0;
            self.bit = 0;
        }
    }

    /// `hcode`: a literal/length symbol in the fixed Huffman code.
    fn hcode(&mut self, v: u32) {
        match v {
            0..=143 => self.write_bits(v + 0x30, 8, true),
            144..=255 => self.write_bits(v - 144 + 0x190, 9, true),
            256..=279 => self.write_bits(v - 256, 7, true),
            _ => self.write_bits(v - 280 + 0xc0, 8, true),
        }
    }

    fn byte(&mut self, x: u8) {
        self.hcode(u32::from(x));
    }

    fn codex(&mut self, c: u32, val: u32, nx: u32) {
        self.hcode(c + (val >> nx));
        self.write_bits(val & ((1 << nx) - 1), nx, false);
    }

    /// `repeat`: copy `n` bytes from `d` back, in runs of at most 258.
    fn repeat(&mut self, mut n: usize, d: usize) {
        while n >= 258 + 3 {
            self.repeat1(258, d);
            n -= 258;
        }
        if n > 258 {
            self.repeat1(10, d);
            self.repeat1(n - 10, d);
            return;
        }
        self.repeat1(n, d);
    }

    /// `repeat1`: one length/distance pair.
    fn repeat1(&mut self, n: usize, d: usize) {
        let n = n as u32;
        match n {
            0..=10 => self.codex(257, n - 3, 0),
            11..=18 => self.codex(265, n - 11, 1),
            19..=34 => self.codex(269, n - 19, 2),
            35..=66 => self.codex(273, n - 35, 3),
            67..=130 => self.codex(277, n - 67, 4),
            131..=257 => self.codex(281, n - 131, 5),
            _ => self.hcode(285),
        }
        let d = d as u32;
        if d <= 4 {
            self.write_bits(d - 1, 5, true);
        } else {
            let mut nbit = 16u32;
            while d <= 1 << (nbit - 1) {
                nbit -= 1;
            }
            let mut v = d - 1;
            v &= !(1 << (nbit - 1));
            let mut code = 2 * nbit - 2;
            code |= v >> (nbit - 2);
            v &= !(1 << (nbit - 2));
            self.write_bits(code, 5, true);
            self.write_bits(v, nbit - 2, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_ieee() {
        let mut crc = Crc32::default();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xcbf4_3926);
    }

    #[test]
    fn the_closed_form_adler_matches_the_byte_loop() {
        let mut fast = Adler::default();
        fast.write_n_byte(255, 1000);
        fast.write_n_byte(0, 3);
        let mut slow = Adler::default();
        slow.write_n(&[255], 1000);
        slow.write_n(&[0], 3);
        assert_eq!(fast.sum32(), slow.sum32());
    }
}
