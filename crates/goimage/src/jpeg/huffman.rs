//! Port of Go's `image/jpeg/huffman.go` (go1.26.4): DHT processing, the 8-bit look-up table plus
//! the slow bit-by-bit path, and the bit-reading helpers shared by the scan decoder.

use super::reader::{Decoder, ERR_MISSING_FF00, JpegError, MAX_TH};

const MAX_CODE_LENGTH: usize = 16;
const MAX_N_CODES: usize = 256;
const LUT_SIZE: u32 = 8;

/// Port of `huffman` (huffman.go:22).
#[derive(Clone)]
pub(crate) struct Huffman {
    n_codes: i32,
    /// High 8 bits: the value; low 8 bits: 1 + code length, or 0 if longer than 8 bits.
    lut: [u16; 1 << LUT_SIZE],
    vals: [u8; MAX_N_CODES],
    min_codes: [i32; MAX_CODE_LENGTH],
    max_codes: [i32; MAX_CODE_LENGTH],
    vals_indices: [i32; MAX_CODE_LENGTH],
}

impl Default for Huffman {
    fn default() -> Self {
        Huffman {
            n_codes: 0,
            lut: [0; 1 << LUT_SIZE],
            vals: [0; MAX_N_CODES],
            min_codes: [0; MAX_CODE_LENGTH],
            max_codes: [0; MAX_CODE_LENGTH],
            vals_indices: [0; MAX_CODE_LENGTH],
        }
    }
}

/// `errShortHuffmanData` (huffman.go:44).
const ERR_SHORT_HUFFMAN_DATA: JpegError = JpegError::Format("short Huffman data");

/// Go's `x >> s` on a `uint32`, which is 0 for a shift of 32 or more (Rust would panic).
fn shr(x: u32, s: i32) -> u32 {
    if (0..32).contains(&s) { x >> s } else { 0 }
}

/// A table selector: which of `d.huff[tc][th]`.
#[derive(Clone, Copy)]
pub(crate) struct Table {
    pub(crate) tc: usize,
    pub(crate) th: usize,
}

impl Decoder<'_> {
    /// Port of `ensureNBits` (huffman.go:49).
    pub(crate) fn ensure_n_bits(&mut self, n: i32) -> Result<(), JpegError> {
        loop {
            let c = match self.read_byte_stuffed_byte() {
                Ok(c) => c,
                Err(JpegError::UnexpectedEof) => return Err(ERR_SHORT_HUFFMAN_DATA),
                Err(e) => return Err(e),
            };
            self.bits.a = self.bits.a << 8 | u32::from(c);
            self.bits.n += 8;
            if self.bits.m == 0 {
                self.bits.m = 1 << 7;
            } else {
                self.bits.m <<= 8;
            }
            if self.bits.n >= n {
                break;
            }
        }
        Ok(())
    }

    /// Port of `receiveExtend` (huffman.go:75): RECEIVE and EXTEND, section F.2.2.1.
    pub(crate) fn receive_extend(&mut self, t: u8) -> Result<i32, JpegError> {
        if self.bits.n < i32::from(t) {
            self.ensure_n_bits(i32::from(t))?;
        }
        self.bits.n -= i32::from(t);
        self.bits.m = shr(self.bits.m, i32::from(t));
        let s = 1i32.wrapping_shl(u32::from(t));
        let mut x = (shr(self.bits.a, self.bits.n) as i32) & s.wrapping_sub(1);
        if x < s >> 1 {
            x = x.wrapping_add((-1i32).wrapping_shl(u32::from(t)).wrapping_add(1));
        }
        Ok(x)
    }

    /// Port of `processDHT` (huffman.go:93), section B.2.4.2.
    pub(crate) fn process_dht(&mut self, mut n: isize) -> Result<(), JpegError> {
        while n > 0 {
            if n < 17 {
                return Err(JpegError::Format("DHT has wrong length"));
            }
            self.read_full_tmp(0, 17)?;
            let tc = self.tmp[0] >> 4;
            if tc > 1 {
                return Err(JpegError::Format("bad Tc value"));
            }
            let th = self.tmp[0] & 0x0f;
            if th > MAX_TH || (self.baseline && th > 1) {
                return Err(JpegError::Format("bad Th value"));
            }
            let (tc, th) = (usize::from(tc), usize::from(th));

            let mut n_codes = [0i32; MAX_CODE_LENGTH];
            let mut total = 0i32;
            for (i, nc) in n_codes.iter_mut().enumerate() {
                *nc = i32::from(self.tmp[i + 1]);
                total += *nc;
            }
            self.huff[tc][th].n_codes = total;
            if total == 0 {
                return Err(JpegError::Format("Huffman table has zero length"));
            }
            if total > MAX_N_CODES as i32 {
                return Err(JpegError::Format("Huffman table has excessive length"));
            }
            n -= total as isize + 17;
            if n < 0 {
                return Err(JpegError::Format("DHT has wrong length"));
            }
            let mut vals = [0u8; MAX_N_CODES];
            vals.copy_from_slice(&self.huff[tc][th].vals);
            self.read_full_into(&mut vals[..total as usize])?;
            let h = &mut self.huff[tc][th];
            h.vals = vals;

            // Derive the look-up table.
            h.lut = [0; 1 << LUT_SIZE];
            let (mut x, mut code) = (0usize, 0u32);
            for i in 0..LUT_SIZE {
                code <<= 1;
                for _ in 0..n_codes[i as usize] {
                    let base = (code << (7 - i)) as u8;
                    let lut_value = u16::from(h.vals[x]) << 8 | (2 + i) as u16;
                    for k in 0..(1u16 << (7 - i)) {
                        h.lut[usize::from(base | k as u8)] = lut_value;
                    }
                    code += 1;
                    x += 1;
                }
            }

            // Derive minCodes, maxCodes, and valsIndices.
            let (mut c, mut index) = (0i32, 0i32);
            for (i, &nc) in n_codes.iter().enumerate() {
                if nc == 0 {
                    h.min_codes[i] = -1;
                    h.max_codes[i] = -1;
                    h.vals_indices[i] = -1;
                } else {
                    h.min_codes[i] = c;
                    h.max_codes[i] = c + nc - 1;
                    h.vals_indices[i] = index;
                    c += nc;
                    index += nc;
                }
                c <<= 1;
            }
        }
        Ok(())
    }

    /// Port of `decodeHuffman` (huffman.go:171).
    pub(crate) fn decode_huffman(&mut self, t: Table) -> Result<u8, JpegError> {
        if self.huff[t.tc][t.th].n_codes == 0 {
            return Err(JpegError::Format("uninitialized Huffman table"));
        }

        let mut slow = false;
        if self.bits.n < 8 {
            if let Err(e) = self.ensure_n_bits(8) {
                if e != ERR_MISSING_FF00 && e != ERR_SHORT_HUFFMAN_DATA {
                    return Err(e);
                }
                // No more bytes in this segment, but the next symbol may still be in the bits
                // already read: undo the readByte ensureNBits made, then go bit by bit.
                if self.bytes.n_unreadable != 0 {
                    self.unread_byte_stuffed_byte();
                }
                slow = true;
            }
        }
        if !slow {
            let idx = (shr(self.bits.a, self.bits.n - LUT_SIZE as i32) & 0xff) as usize;
            let v = self.huff[t.tc][t.th].lut[idx];
            if v != 0 {
                let n = (v & 0xff) - 1;
                self.bits.n -= i32::from(n);
                self.bits.m = shr(self.bits.m, i32::from(n));
                return Ok((v >> 8) as u8);
            }
        }

        let mut code = 0i32;
        for i in 0..MAX_CODE_LENGTH {
            if self.bits.n == 0 {
                self.ensure_n_bits(1)?;
            }
            if self.bits.a & self.bits.m != 0 {
                code |= 1;
            }
            self.bits.n -= 1;
            self.bits.m >>= 1;
            let h = &self.huff[t.tc][t.th];
            if code <= h.max_codes[i] {
                let idx = h.vals_indices[i] + code - h.min_codes[i];
                return usize::try_from(idx)
                    .ok()
                    .and_then(|i| h.vals.get(i).copied())
                    .ok_or(JpegError::Format("bad Huffman code"));
            }
            code <<= 1;
        }
        Err(JpegError::Format("bad Huffman code"))
    }

    /// Port of `decodeBit` (huffman.go:216).
    pub(crate) fn decode_bit(&mut self) -> Result<bool, JpegError> {
        if self.bits.n == 0 {
            self.ensure_n_bits(1)?;
        }
        let ret = self.bits.a & self.bits.m != 0;
        self.bits.n -= 1;
        self.bits.m >>= 1;
        Ok(ret)
    }

    /// Port of `decodeBits` (huffman.go:228).
    pub(crate) fn decode_bits(&mut self, n: i32) -> Result<u32, JpegError> {
        if self.bits.n < n {
            self.ensure_n_bits(n)?;
        }
        let mut ret = shr(self.bits.a, self.bits.n - n);
        ret &= 1u32.wrapping_shl(n as u32).wrapping_sub(1);
        self.bits.n -= n;
        self.bits.m = shr(self.bits.m, n);
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_past_the_word_are_zero_as_in_go() {
        assert_eq!(shr(0xffff_ffff, 32), 0);
        assert_eq!(shr(0xffff_ffff, 31), 1);
        assert_eq!(shr(5, -1), 0);
    }
}
