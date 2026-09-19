//! Port of `compress/flate/huffman_bit_writer.go`: block encoding (stored, fixed, dynamic) and
//! the bit accumulator, including its 240-byte flush threshold — which decides where the
//! downstream writer's `Write` calls fall.

use super::huffman_code::{
    HCode, HuffmanEncoder, MAX_NUM_LIT, fixed_literal_encoding, fixed_offset_encoding,
};
use super::token::{MATCH_TYPE, Token, length, length_code, literal, offset, offset_code};
use crate::sink::Sink;
use std::sync::OnceLock;

const OFFSET_CODE_COUNT: usize = 30;
const END_BLOCK_MARKER: usize = 256;
const LENGTH_CODES_START: usize = 257;
const CODEGEN_CODE_COUNT: usize = 19;
const BAD_CODE: u8 = 255;
const BUFFER_FLUSH_SIZE: usize = 240;
const BUFFER_SIZE: usize = BUFFER_FLUSH_SIZE + 8;
/// `maxStoreBlockSize` (deflate.go:51).
pub(crate) const MAX_STORE_BLOCK_SIZE: usize = 65535;

static LENGTH_EXTRA_BITS: [i8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
static LENGTH_BASE: [u32; 29] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224, 255,
];
static OFFSET_EXTRA_BITS: [i8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
static OFFSET_BASE: [u32; 30] = [
    0x000000, 0x000001, 0x000002, 0x000003, 0x000004, 0x000006, 0x000008, 0x00000c, 0x000010,
    0x000018, 0x000020, 0x000030, 0x000040, 0x000060, 0x000080, 0x0000c0, 0x000100, 0x000180,
    0x000200, 0x000300, 0x000400, 0x000600, 0x000800, 0x000c00, 0x001000, 0x001800, 0x002000,
    0x003000, 0x004000, 0x006000,
];
static CODEGEN_ORDER: [u32; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// `huffOffset` (huffman_bit_writer.go:612): the offset code used by Huffman-only blocks.
fn huff_offset() -> &'static HuffmanEncoder {
    static E: OnceLock<HuffmanEncoder> = OnceLock::new();
    E.get_or_init(|| {
        let mut freq = vec![0i32; OFFSET_CODE_COUNT];
        freq[0] = 1;
        let mut h = HuffmanEncoder::new(OFFSET_CODE_COUNT);
        h.generate(&freq, 15);
        h
    })
}

/// Which literal/offset code pair a block is written with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Fixed,
    Dynamic,
}

/// Port of `huffmanBitWriter` (huffman_bit_writer.go:79). Writes are infallible (see
/// [`crate::sink`]), so Go's sticky `err` has nothing to hold; the one internal error Go can raise
/// here — `writeBytes with unfinished bits` — cannot happen from the compressor's call pattern.
pub(crate) struct HuffmanBitWriter<W: Sink> {
    pub(crate) writer: W,
    bits: u64,
    nbits: u32,
    bytes: [u8; BUFFER_SIZE],
    codegen_freq: [i32; CODEGEN_CODE_COUNT],
    nbytes: usize,
    literal_freq: Vec<i32>,
    offset_freq: Vec<i32>,
    codegen: Vec<u8>,
    literal_encoding: HuffmanEncoder,
    offset_encoding: HuffmanEncoder,
    codegen_encoding: HuffmanEncoder,
}

impl<W: Sink> HuffmanBitWriter<W> {
    /// `newHuffmanBitWriter`.
    pub(crate) fn new(writer: W) -> Self {
        HuffmanBitWriter {
            writer,
            bits: 0,
            nbits: 0,
            bytes: [0; BUFFER_SIZE],
            codegen_freq: [0; CODEGEN_CODE_COUNT],
            nbytes: 0,
            literal_freq: vec![0; MAX_NUM_LIT],
            offset_freq: vec![0; OFFSET_CODE_COUNT],
            codegen: vec![0; MAX_NUM_LIT + OFFSET_CODE_COUNT + 1],
            literal_encoding: HuffmanEncoder::new(MAX_NUM_LIT),
            offset_encoding: HuffmanEncoder::new(OFFSET_CODE_COUNT),
            codegen_encoding: HuffmanEncoder::new(CODEGEN_CODE_COUNT),
        }
    }

    /// `flush` (huffman_bit_writer.go:117).
    pub(crate) fn flush(&mut self) {
        let mut n = self.nbytes;
        while self.nbits != 0 {
            self.bytes[n] = self.bits as u8;
            self.bits >>= 8;
            if self.nbits > 8 {
                self.nbits -= 8;
            } else {
                self.nbits = 0;
            }
            n += 1;
        }
        self.bits = 0;
        self.writer.write(&self.bytes[..n]);
        self.nbytes = 0;
    }

    /// The shared tail of `writeBits`/`writeCode`: spill 6 bytes once 48 bits are pending.
    fn spill48(&mut self) {
        if self.nbits >= 48 {
            let bits = self.bits;
            self.bits >>= 48;
            self.nbits -= 48;
            let mut n = self.nbytes;
            self.bytes[n..n + 6].copy_from_slice(&bits.to_le_bytes()[..6]);
            n += 6;
            if n >= BUFFER_FLUSH_SIZE {
                self.writer.write(&self.bytes[..n]);
                n = 0;
            }
            self.nbytes = n;
        }
    }

    /// `writeBits` (huffman_bit_writer.go:144).
    fn write_bits(&mut self, b: i32, nb: u32) {
        self.bits |= u64::from(b as u32) << self.nbits;
        self.nbits += nb;
        self.spill48();
    }

    /// `writeCode` (huffman_bit_writer.go:375).
    fn write_code(&mut self, c: HCode) {
        self.bits |= u64::from(c.code) << self.nbits;
        self.nbits += u32::from(c.len);
        self.spill48();
    }

    /// `writeBytes` (huffman_bit_writer.go:170).
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        let mut n = self.nbytes;
        // Go returns InternalError("writeBytes with unfinished bits") when nbits&7 != 0; the
        // compressor only calls this right after writeStoredHeader, which flushes to a byte.
        debug_assert_eq!(self.nbits & 7, 0);
        while self.nbits != 0 {
            self.bytes[n] = self.bits as u8;
            self.bits >>= 8;
            self.nbits -= 8;
            n += 1;
        }
        if n != 0 {
            self.writer.write(&self.bytes[..n]);
        }
        self.nbytes = 0;
        self.writer.write(bytes);
    }

    /// `generateCodegen` (huffman_bit_writer.go:203) over `literalEncoding` and either
    /// `offsetEncoding` or, for `huff`, `huffOffset`.
    fn generate_codegen(&mut self, num_literals: usize, num_offsets: usize, huff: bool) {
        self.codegen_freq = [0; CODEGEN_CODE_COUNT];
        let lit_enc = &self.literal_encoding;
        let off_enc = if huff {
            huff_offset()
        } else {
            &self.offset_encoding
        };
        let codegen = &mut self.codegen;
        for (dst, c) in codegen[..num_literals].iter_mut().zip(&lit_enc.codes) {
            *dst = c.len as u8;
        }
        for (dst, c) in codegen[num_literals..num_literals + num_offsets]
            .iter_mut()
            .zip(&off_enc.codes)
        {
            *dst = c.len as u8;
        }
        codegen[num_literals + num_offsets] = BAD_CODE;

        let mut size = codegen[0];
        let mut count: i32 = 1;
        let mut out_index = 0usize;
        let mut in_index = 1usize;
        while size != BAD_CODE {
            let next_size = codegen[in_index];
            in_index += 1;
            if next_size == size {
                count += 1;
                continue;
            }
            if size != 0 {
                codegen[out_index] = size;
                out_index += 1;
                self.codegen_freq[size as usize] += 1;
                count -= 1;
                while count >= 3 {
                    let n = count.min(6);
                    codegen[out_index] = 16;
                    out_index += 1;
                    codegen[out_index] = (n - 3) as u8;
                    out_index += 1;
                    self.codegen_freq[16] += 1;
                    count -= n;
                }
            } else {
                while count >= 11 {
                    let n = count.min(138);
                    codegen[out_index] = 18;
                    out_index += 1;
                    codegen[out_index] = (n - 11) as u8;
                    out_index += 1;
                    self.codegen_freq[18] += 1;
                    count -= n;
                }
                if count >= 3 {
                    codegen[out_index] = 17;
                    out_index += 1;
                    codegen[out_index] = (count - 3) as u8;
                    out_index += 1;
                    self.codegen_freq[17] += 1;
                    count = 0;
                }
            }
            count -= 1;
            while count >= 0 {
                codegen[out_index] = size;
                out_index += 1;
                self.codegen_freq[size as usize] += 1;
                count -= 1;
            }
            size = next_size;
            count = 1;
        }
        codegen[out_index] = BAD_CODE;
    }

    /// `dynamicSize` (huffman_bit_writer.go:294). `huff` selects `huffOffset` as the offset code.
    fn dynamic_size(&self, huff: bool, extra_bits: i64) -> (i64, usize) {
        let mut num_codegens = self.codegen_freq.len();
        while num_codegens > 4 && self.codegen_freq[CODEGEN_ORDER[num_codegens - 1] as usize] == 0 {
            num_codegens -= 1;
        }
        let header = 3
            + 5
            + 5
            + 4
            + 3 * num_codegens as i64
            + self.codegen_encoding.bit_length(&self.codegen_freq)
            + i64::from(self.codegen_freq[16]) * 2
            + i64::from(self.codegen_freq[17]) * 3
            + i64::from(self.codegen_freq[18]) * 7;
        let off_enc = if huff {
            huff_offset()
        } else {
            &self.offset_encoding
        };
        let size = header
            + self.literal_encoding.bit_length(&self.literal_freq)
            + off_enc.bit_length(&self.offset_freq)
            + extra_bits;
        (size, num_codegens)
    }

    /// `fixedSize` (huffman_bit_writer.go:314).
    fn fixed_size(&self, extra_bits: i64) -> i64 {
        3 + fixed_literal_encoding().bit_length(&self.literal_freq)
            + fixed_offset_encoding().bit_length(&self.offset_freq)
            + extra_bits
    }

    /// `storedSize` (huffman_bit_writer.go:324).
    fn stored_size(input: Option<&[u8]>) -> (i64, bool) {
        match input {
            Some(i) if i.len() <= MAX_STORE_BLOCK_SIZE => (((i.len() + 5) * 8) as i64, true),
            _ => (0, false),
        }
    }

    /// `writeDynamicHeader` (huffman_bit_writer.go:402).
    fn write_dynamic_header(
        &mut self,
        num_literals: usize,
        num_offsets: usize,
        num_codegens: usize,
        is_eof: bool,
    ) {
        let first_bits = if is_eof { 5 } else { 4 };
        self.write_bits(first_bits, 3);
        self.write_bits((num_literals - 257) as i32, 5);
        self.write_bits((num_offsets - 1) as i32, 5);
        self.write_bits((num_codegens - 4) as i32, 4);
        for &order in &CODEGEN_ORDER[..num_codegens] {
            let value = self.codegen_encoding.codes[order as usize].len;
            self.write_bits(i32::from(value), 3);
        }
        let mut i = 0;
        loop {
            let code_word = self.codegen[i];
            i += 1;
            if code_word == BAD_CODE {
                break;
            }
            self.write_code(self.codegen_encoding.codes[code_word as usize]);
            match code_word {
                16 => {
                    self.write_bits(i32::from(self.codegen[i]), 2);
                    i += 1;
                }
                17 => {
                    self.write_bits(i32::from(self.codegen[i]), 3);
                    i += 1;
                }
                18 => {
                    self.write_bits(i32::from(self.codegen[i]), 7);
                    i += 1;
                }
                _ => {}
            }
        }
    }

    /// `writeStoredHeader` (huffman_bit_writer.go:447).
    pub(crate) fn write_stored_header(&mut self, length: usize, is_eof: bool) {
        let flag = i32::from(is_eof);
        self.write_bits(flag, 3);
        self.flush();
        self.write_bits(length as i32, 16);
        self.write_bits(i32::from(!(length as u16)), 16);
    }

    /// `writeFixedHeader` (huffman_bit_writer.go:461).
    fn write_fixed_header(&mut self, is_eof: bool) {
        let value = if is_eof { 3 } else { 2 };
        self.write_bits(value, 3);
    }

    /// Port of `writeBlock` (huffman_bit_writer.go:477): the smallest of fixed, dynamic and stored.
    /// `input` is `nil` in Go when the block's bytes are no longer in the window.
    pub(crate) fn write_block(&mut self, tokens: &mut Vec<Token>, eof: bool, input: Option<&[u8]>) {
        tokens.push(END_BLOCK_MARKER as Token);
        let (num_literals, num_offsets) = self.index_tokens(tokens);

        let mut extra_bits = 0i64;
        let (stored_size, storable) = Self::stored_size(input);
        if storable {
            for lc in LENGTH_CODES_START + 8..num_literals {
                extra_bits += i64::from(self.literal_freq[lc])
                    * i64::from(LENGTH_EXTRA_BITS[lc - LENGTH_CODES_START]);
            }
            for (freq, &extra) in self.offset_freq[4..num_offsets.max(4)]
                .iter()
                .zip(&OFFSET_EXTRA_BITS[4..])
            {
                extra_bits += i64::from(*freq) * i64::from(extra);
            }
        }

        let mut encoding = Encoding::Fixed;
        let mut size = self.fixed_size(extra_bits);

        self.generate_codegen(num_literals, num_offsets, false);
        self.codegen_encoding.generate(&self.codegen_freq, 7);
        let (dynamic_size, num_codegens) = self.dynamic_size(false, extra_bits);

        if dynamic_size < size {
            size = dynamic_size;
            encoding = Encoding::Dynamic;
        }

        if let Some(input) = input {
            if storable && stored_size < size {
                self.write_stored_header(input.len(), eof);
                self.write_bytes(input);
                tokens.pop();
                return;
            }
        }

        match encoding {
            Encoding::Fixed => {
                self.write_fixed_header(eof);
                self.write_tokens(tokens, Encoding::Fixed);
            }
            Encoding::Dynamic => {
                self.write_dynamic_header(num_literals, num_offsets, num_codegens, eof);
                self.write_tokens(tokens, Encoding::Dynamic);
            }
        }
        // Go appends to a slice of the caller's backing array and never shows the caller the
        // marker; the Rust caller's Vec must not keep it either.
        tokens.pop();
    }

    /// `indexTokens` (huffman_bit_writer.go:574).
    fn index_tokens(&mut self, tokens: &[Token]) -> (usize, usize) {
        self.literal_freq.fill(0);
        self.offset_freq.fill(0);
        for &t in tokens {
            if t < MATCH_TYPE {
                self.literal_freq[literal(t) as usize] += 1;
                continue;
            }
            self.literal_freq[LENGTH_CODES_START + length_code(length(t)) as usize] += 1;
            self.offset_freq[offset_code(offset(t)) as usize] += 1;
        }
        let mut num_literals = self.literal_freq.len();
        while self.literal_freq[num_literals - 1] == 0 {
            num_literals -= 1;
        }
        let mut num_offsets = self.offset_freq.len();
        while num_offsets > 0 && self.offset_freq[num_offsets - 1] == 0 {
            num_offsets -= 1;
        }
        if num_offsets == 0 {
            self.offset_freq[0] = 1;
            num_offsets = 1;
        }
        self.literal_encoding.generate(&self.literal_freq, 15);
        self.offset_encoding.generate(&self.offset_freq, 15);
        (num_literals, num_offsets)
    }

    /// `writeTokens` (huffman_bit_writer.go:612).
    fn write_tokens(&mut self, tokens: &[Token], encoding: Encoding) {
        for &t in tokens {
            let (lit, off) = match encoding {
                Encoding::Fixed => (fixed_literal_encoding(), fixed_offset_encoding()),
                Encoding::Dynamic => (&self.literal_encoding, &self.offset_encoding),
            };
            if t < MATCH_TYPE {
                let c = lit.codes[literal(t) as usize];
                self.write_code(c);
                continue;
            }
            let len = length(t);
            let lc = length_code(len) as usize;
            let len_code = lit.codes[lc + LENGTH_CODES_START];
            let off_t = offset(t);
            let oc = offset_code(off_t) as usize;
            let off_code = off.codes[oc];
            self.write_code(len_code);
            let extra_length_bits = LENGTH_EXTRA_BITS[lc] as u32;
            if extra_length_bits > 0 {
                self.write_bits((len - LENGTH_BASE[lc]) as i32, extra_length_bits);
            }
            self.write_code(off_code);
            let extra_offset_bits = OFFSET_EXTRA_BITS[oc] as u32;
            if extra_offset_bits > 0 {
                self.write_bits((off_t - OFFSET_BASE[oc]) as i32, extra_offset_bits);
            }
        }
    }

    /// Port of `writeBlockHuff` (huffman_bit_writer.go:625): literals only, or stored.
    pub(crate) fn write_block_huff(&mut self, eof: bool, input: &[u8]) {
        self.literal_freq.fill(0);
        for &t in input {
            self.literal_freq[t as usize] += 1;
        }
        self.literal_freq[END_BLOCK_MARKER] = 1;
        const NUM_LITERALS: usize = END_BLOCK_MARKER + 1;
        self.offset_freq[0] = 1;
        const NUM_OFFSETS: usize = 1;

        self.literal_encoding.generate(&self.literal_freq, 15);
        self.generate_codegen(NUM_LITERALS, NUM_OFFSETS, true);
        self.codegen_encoding.generate(&self.codegen_freq, 7);
        let (size, num_codegens) = self.dynamic_size(true, 0);

        let (ssize, storable) = Self::stored_size(Some(input));
        if storable && ssize < size + (size >> 4) {
            self.write_stored_header(input.len(), eof);
            self.write_bytes(input);
            return;
        }

        self.write_dynamic_header(NUM_LITERALS, NUM_OFFSETS, num_codegens, eof);
        let mut n = self.nbytes;
        for &t in input {
            let c = self.literal_encoding.codes[t as usize];
            self.bits |= u64::from(c.code) << self.nbits;
            self.nbits += u32::from(c.len);
            if self.nbits < 48 {
                continue;
            }
            let bits = self.bits;
            self.bits >>= 48;
            self.nbits -= 48;
            self.bytes[n..n + 6].copy_from_slice(&bits.to_le_bytes()[..6]);
            n += 6;
            if n < BUFFER_FLUSH_SIZE {
                continue;
            }
            self.writer.write(&self.bytes[..n]);
            n = 0;
        }
        self.nbytes = n;
        let end = self.literal_encoding.codes[END_BLOCK_MARKER];
        self.write_code(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Calls(Vec<usize>);
    impl Sink for Calls {
        fn write(&mut self, p: &[u8]) {
            self.0.push(p.len());
        }
    }

    /// The accumulator hands its buffer on at exactly 240 bytes (`bufferFlushSize`,
    /// huffman_bit_writer.go:31). The byte stream would be the same at any threshold; the
    /// downstream `Write` boundaries would not.
    #[test]
    fn the_bit_buffer_spills_in_240_byte_writes() {
        // Text-like input compresses into dynamic blocks, whose codes go through writeCode and
        // writeBits; writeBlockHuff has its own copy of the spill and is covered separately.
        let words = [
            &b"image "[..],
            b"preview ",
            b"a ",
            b"channel ",
            b"mattermost ",
        ];
        let mut input = Vec::new();
        for i in 0..20_000 {
            input.extend_from_slice(words[(crate::testsupport::hash4(5, i, 0, 0) % 5) as usize]);
        }
        let mut w = crate::flate::Writer::new(Calls::default(), 9).unwrap();
        w.write(&input);
        w.close();
        let calls = w.into_inner().0;
        assert!(calls.len() > 10, "{calls:?}");
        // The last two are Close's flushes around the final stored header.
        assert!(
            calls[..calls.len() - 2].iter().all(|&n| n == 240),
            "{calls:?}"
        );
    }

    #[test]
    fn huffman_only_blocks_spill_in_240_byte_writes_too() {
        let input: Vec<u8> = (0..20_000u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 24) % 7) as u8)
            .collect();
        let mut w = HuffmanBitWriter::new(Calls::default());
        w.write_block_huff(true, &input);
        w.flush();
        let calls = &w.writer.0;
        assert!(calls.len() > 10, "{calls:?}");
        assert!(
            calls[..calls.len() - 1].iter().all(|&n| n == 240),
            "{calls:?}"
        );
    }
}
