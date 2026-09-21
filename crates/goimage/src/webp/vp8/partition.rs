//! Port of `golang.org/x/image/vp8/partition.go`: decoding one arithmetic-coded partition's
//! bitstream (chapter 7), following libwebp's look-up-table recalibration rather than the
//! specification's for loop.

/// `lutShift`.
const LUT_SHIFT: [u8; 127] = [
    7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
];

/// `lutRangeM1`.
const LUT_RANGE_M1: [u8; 127] = [
    127, //
    127, 191, //
    127, 159, 191, 223, //
    127, 143, 159, 175, 191, 207, 223, 239, //
    127, 135, 143, 151, 159, 167, 175, 183, 191, 199, 207, 215, 223, 231, 239, 247, //
    127, 131, 135, 139, 143, 147, 151, 155, 159, 163, 167, 171, 175, 179, 183, 187, //
    191, 195, 199, 203, 207, 211, 215, 219, 223, 227, 231, 235, 239, 243, 247, 251, //
    127, 129, 131, 133, 135, 137, 139, 141, 143, 145, 147, 149, 151, 153, 155, 157, //
    159, 161, 163, 165, 167, 169, 171, 173, 175, 177, 179, 181, 183, 185, 187, 189, //
    191, 193, 195, 197, 199, 201, 203, 205, 207, 209, 211, 213, 215, 217, 219, 221, //
    223, 225, 227, 229, 231, 233, 235, 237, 239, 241, 243, 245, 247, 249, 251, 253, //
];

/// `uniformProb`: a 50% probability that the next bit is 0.
pub(super) const UNIFORM_PROB: u8 = 128;

/// `partition`: arithmetic-coded bits over one slice of the frame.
#[derive(Default)]
pub(super) struct Partition {
    /// The input bytes.
    buf: Vec<u8>,
    /// How many of `buf`'s bytes have been consumed.
    r: usize,
    /// Range minus one, in the arithmetic-coding sense.
    range_m1: u32,
    /// Bits shifted out of `buf` but not yet consumed, and how many of them there are.
    bits: u32,
    n_bits: u8,
    /// Whether we tried to read past `buf`.
    pub(super) unexpected_eof: bool,
}

impl Partition {
    /// `partition.init`.
    pub(super) fn init(&mut self, buf: Vec<u8>) {
        self.buf = buf;
        self.r = 0;
        self.range_m1 = 254;
        self.bits = 0;
        self.n_bits = 0;
        self.unexpected_eof = false;
    }

    /// `partition.readBit`.
    ///
    /// The shifts are Go's on `uint32`/`uint8`: `p.bits <<= shift` drops the high bits and
    /// `p.nBits -= shift` cannot wrap, because the look-up table only asks for a shift that is at
    /// most the eight bits the branch above guarantees.
    pub(super) fn read_bit(&mut self, prob: u8) -> bool {
        if self.n_bits < 8 {
            if self.r >= self.buf.len() {
                self.unexpected_eof = true;
                return false;
            }
            let x = u32::from(self.buf[self.r]);
            self.bits |= x << (8 - self.n_bits);
            self.r += 1;
            self.n_bits += 8;
        }
        let split = ((self.range_m1 * u32::from(prob)) >> 8) + 1;
        let bit = self.bits >= split << 8;
        if bit {
            self.range_m1 -= split;
            self.bits -= split << 8;
        } else {
            self.range_m1 = split - 1;
        }
        if self.range_m1 < 127 {
            let shift = LUT_SHIFT[self.range_m1 as usize];
            self.range_m1 = u32::from(LUT_RANGE_M1[self.range_m1 as usize]);
            self.bits <<= shift;
            self.n_bits -= shift;
        }
        bit
    }

    /// `partition.readUint`.
    pub(super) fn read_uint(&mut self, prob: u8, mut n: u8) -> u32 {
        let mut u = 0u32;
        while n > 0 {
            n -= 1;
            if self.read_bit(prob) {
                u |= 1 << n;
            }
        }
        u
    }

    /// `partition.readInt`.
    pub(super) fn read_int(&mut self, prob: u8, n: u8) -> i32 {
        let u = self.read_uint(prob, n);
        if self.read_bit(prob) {
            -(u as i32)
        } else {
            u as i32
        }
    }

    /// `partition.readOptionalInt`: an n-bit signed integer whose likely value is zero.
    pub(super) fn read_optional_int(&mut self, prob: u8, n: u8) -> i32 {
        if !self.read_bit(prob) {
            return 0;
        }
        self.read_int(prob, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recalibration_tables_have_gos_shape() {
        assert_eq!(LUT_SHIFT.len(), 127);
        assert_eq!(LUT_RANGE_M1.len(), 127);
        assert_eq!(LUT_SHIFT[0], 7);
        assert_eq!(LUT_SHIFT[126], 1);
        assert_eq!(LUT_RANGE_M1[0], 127);
        assert_eq!(LUT_RANGE_M1[126], 253);
        // Every entry is the smallest power-of-two scaling that lifts range past 127.
        for (i, &s) in LUT_SHIFT.iter().enumerate() {
            let scaled = (i as u32 + 1) << s;
            assert!((128..256).contains(&scaled), "{i}");
            assert_eq!(LUT_RANGE_M1[i] as u32, scaled - 1, "{i}");
        }
    }

    #[test]
    fn an_empty_partition_reports_an_unexpected_eof_and_reads_zeroes() {
        let mut p = Partition::default();
        p.init(Vec::new());
        assert!(!p.read_bit(UNIFORM_PROB));
        assert!(p.unexpected_eof);
        assert_eq!(p.read_uint(UNIFORM_PROB, 8), 0);
        assert_eq!(p.read_int(UNIFORM_PROB, 4), 0);
        assert_eq!(p.read_optional_int(UNIFORM_PROB, 4), 0);
    }

    #[test]
    fn a_uniform_stream_of_ones_reads_back_as_ones() {
        // With prob 128 and range 254, split is 128: a first byte of 0xff is above the split, so
        // the first bit is 1.
        let mut p = Partition::default();
        p.init(vec![0xff; 8]);
        assert!(p.read_bit(UNIFORM_PROB));
        assert!(!p.unexpected_eof);
        let mut p = Partition::default();
        p.init(vec![0x00; 8]);
        assert!(!p.read_bit(UNIFORM_PROB));
        assert!(!p.unexpected_eof);
    }

    #[test]
    fn read_int_is_sign_and_magnitude() {
        // 0x7f is 0b01111111: with prob 128 the arithmetic coder tracks the raw bits closely
        // enough that the magnitude and the sign bit come out of the same stream; this pins the
        // shape of readInt against readUint rather than a hand-computed value.
        let mut a = Partition::default();
        a.init(vec![0x7f, 0x30, 0x91, 0x00]);
        let mag = a.read_uint(UNIFORM_PROB, 4);
        let neg = a.read_bit(UNIFORM_PROB);
        let mut b = Partition::default();
        b.init(vec![0x7f, 0x30, 0x91, 0x00]);
        let got = b.read_int(UNIFORM_PROB, 4);
        assert_eq!(got, if neg { -(mag as i32) } else { mag as i32 });
    }
}
