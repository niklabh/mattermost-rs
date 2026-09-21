//! Port of `golang.org/x/image/vp8l/huffman.go`: the canonical Huffman trees VP8L's entropy coder
//! reads symbols from, and the 7-bit look-up table that short-circuits the tree walk.

use super::{BitReader, Error};
use crate::goread;

/// `reverseBits` (huffman.go:12). Go spells the 256 entries out; `u8::reverse_bits` is the same
/// function, and the test below pins a row of Go's literal against it.
const REVERSE_BITS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = (i as u8).reverse_bits();
        i += 1;
    }
    t
};

/// `hNode`: a node in a Huffman tree.
#[derive(Clone, Copy, Debug, Default)]
struct HNode {
    /// The symbol held by this node.
    symbol: u32,
    /// The `nodes` index of the first of this node's two children, if positive. Zero means an
    /// uninitialized node, and -1 a leaf.
    children: i32,
}

const LEAF_NODE: i32 = -1;

/// `lutSize`: the log-2 size of an `HTree`'s look-up table.
const LUT_SIZE: u32 = 7;
const LUT_MASK: u32 = (1 << LUT_SIZE) - 1;

/// `hTree`: a Huffman tree.
///
/// `lut[x]`, for the next `LUT_SIZE` bits `x`, holds in its low eight bits one plus the length of
/// the next code, or zero when the code needs more than `LUT_SIZE` bits; the high 24 bits are the
/// symbol in the first case and the node index to start walking from in the second.
pub(super) struct HTree {
    nodes: Vec<HNode>,
    /// Go's `cap(h.nodes)`, which `insert` tests against: `Vec::capacity` may exceed what was
    /// asked for, so the bound is kept separately.
    cap: usize,
    lut: [u32; 1 << LUT_SIZE],
}

impl Default for HTree {
    /// Go's zero `hTree`: a nil node slice and a zeroed look-up table.
    fn default() -> Self {
        HTree {
            nodes: Vec::new(),
            cap: 0,
            lut: [0; 1 << LUT_SIZE],
        }
    }
}

impl HTree {
    /// `hTree.insert`: a symbol whose encoding is the least significant `code_length` bits of
    /// `code`.
    fn insert(&mut self, symbol: u32, code: u32, code_length: u32) -> Result<(), Error> {
        if symbol > 0xffff || code_length > 0xfe {
            return Err(Error::InvalidHuffmanTree);
        }
        // Every caller keeps `code_length` at 15 or below (`codeLengthsToCodes` rejects more, and
        // the simple and trivial trees use 0 or 1), so none of the shifts below can reach 32.
        let base_code = if code_length > LUT_SIZE {
            u32::from(REVERSE_BITS[((code >> (code_length - LUT_SIZE)) & 0xff) as usize])
                >> (8 - LUT_SIZE)
        } else {
            let base_code = u32::from(REVERSE_BITS[(code & 0xff) as usize]) >> (8 - code_length);
            for i in 0..1u32 << (LUT_SIZE - code_length) {
                self.lut[(base_code | i << code_length) as usize] = symbol << 8 | (code_length + 1);
            }
            base_code
        };

        let mut n: usize = 0;
        // Signed, as in Go: a code longer than LUT_SIZE drives `jump` past zero.
        let mut jump = LUT_SIZE as i32;
        let mut cl = code_length;
        while cl > 0 {
            cl -= 1;
            if n > self.nodes.len() {
                return Err(Error::InvalidHuffmanTree);
            }
            match self.node(n)?.children {
                LEAF_NODE => return Err(Error::InvalidHuffmanTree),
                0 => {
                    if self.nodes.len() == self.cap {
                        return Err(Error::InvalidHuffmanTree);
                    }
                    // Create two empty child nodes. `len` is odd and `cap` odd throughout, so the
                    // test above stops exactly at the boundary and the pushes stay inside it.
                    self.nodes[n].children = self.nodes.len() as i32;
                    self.nodes.push(HNode::default());
                    self.nodes.push(HNode::default());
                }
                _ => {}
            }
            n = (self.node(n)?.children as usize) + (1 & (code >> cl)) as usize;
            jump -= 1;
            if jump == 0 && self.lut[base_code as usize] == 0 {
                self.lut[base_code as usize] = (n as u32) << 8;
            }
        }

        match self.node(n)?.children {
            LEAF_NODE => {}
            // Turn the uninitialized node into a leaf.
            0 => self.nodes[n].children = LEAF_NODE,
            _ => return Err(Error::InvalidHuffmanTree),
        }
        self.node_mut(n)?.symbol = symbol;
        Ok(())
    }

    /// Go indexes `h.nodes` directly and panics past its end. Every index reached here is one
    /// `insert` itself just allocated, so the error branch is unreachable; it exists so that a
    /// future change cannot turn a bad stream into a panic.
    fn node(&self, n: usize) -> Result<HNode, Error> {
        self.nodes.get(n).copied().ok_or(Error::InvalidHuffmanTree)
    }

    fn node_mut(&mut self, n: usize) -> Result<&mut HNode, Error> {
        self.nodes.get_mut(n).ok_or(Error::InvalidHuffmanTree)
    }

    /// `hTree.build`: a canonical Huffman tree from the given code lengths.
    pub(super) fn build(&mut self, code_lengths: &[u32]) -> Result<(), Error> {
        let mut n_symbols: u32 = 0;
        let mut last_symbol: u32 = 0;
        for (symbol, &cl) in code_lengths.iter().enumerate() {
            if cl != 0 {
                n_symbols += 1;
                last_symbol = symbol as u32;
            }
        }
        if n_symbols == 0 {
            return Err(Error::InvalidHuffmanTree);
        }
        self.cap = (2 * n_symbols - 1) as usize;
        self.nodes = Vec::with_capacity(self.cap);
        self.nodes.push(HNode::default());
        // The trivial case.
        if n_symbols == 1 {
            if code_lengths.len() <= last_symbol as usize {
                return Err(Error::InvalidHuffmanTree);
            }
            return self.insert(last_symbol, 0, 0);
        }
        // The non-trivial case.
        let codes = code_lengths_to_codes(code_lengths)?;
        for (symbol, &cl) in code_lengths.iter().enumerate() {
            if cl > 0 {
                self.insert(symbol as u32, codes[symbol], cl)?;
            }
        }
        Ok(())
    }

    /// `hTree.buildSimple`: a tree with one or two symbols.
    pub(super) fn build_simple(
        &mut self,
        n_symbols: u32,
        symbols: [u32; 2],
        alphabet_size: u32,
    ) -> Result<(), Error> {
        self.cap = (2 * n_symbols - 1) as usize;
        self.nodes = Vec::with_capacity(self.cap);
        self.nodes.push(HNode::default());
        for i in 0..n_symbols {
            if symbols[i as usize] >= alphabet_size {
                return Err(Error::InvalidHuffmanTree);
            }
            self.insert(symbols[i as usize], i, n_symbols - 1)?;
        }
        Ok(())
    }

    /// `hTree.next`: the next Huffman-encoded symbol from the bit-stream.
    pub(super) fn next(&self, d: &mut BitReader<'_>) -> Result<u32, Error> {
        let mut n: usize = 0;
        // Read enough bits so that we can use the look-up table.
        let mut slow_path = false;
        if d.n_bits < LUT_SIZE {
            match d.read_byte() {
                Ok(c) => {
                    d.bits |= u32::from(c) << d.n_bits;
                    d.n_bits += 8;
                }
                // There are no more bytes of data, but we may still be able to read the next
                // symbol out of the previously read bits.
                Err(goread::Error::Eof) => slow_path = true,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        if !slow_path {
            // Use the look-up table.
            let v = self.lut[(d.bits & LUT_MASK) as usize];
            let b = v & 0xff;
            if b != 0 {
                // `b - 1` is the code's length, at most LUT_SIZE, and `n_bits` is at least
                // LUT_SIZE on this path, so neither the shift nor the subtraction can wrap.
                let b = b - 1;
                d.bits >>= b;
                d.n_bits -= b;
                return Ok(v >> 8);
            }
            n = (v >> 8) as usize;
            d.bits >>= LUT_SIZE;
            d.n_bits -= LUT_SIZE;
        }

        while self.node(n)?.children != LEAF_NODE {
            if d.n_bits == 0 {
                let c = match d.read_byte() {
                    Ok(c) => c,
                    Err(goread::Error::Eof) => return Err(Error::Io(goread::Error::UnexpectedEof)),
                    Err(e) => return Err(Error::Io(e)),
                };
                d.bits = u32::from(c);
                d.n_bits = 8;
            }
            n = (self.node(n)?.children as usize) + (1 & d.bits) as usize;
            d.bits >>= 1;
            d.n_bits -= 1;
        }
        Ok(self.node(n)?.symbol)
    }
}

/// `codeLengthsToCodes`: the canonical Huffman codes a sequence of code lengths implies.
fn code_lengths_to_codes(code_lengths: &[u32]) -> Result<Vec<u32>, Error> {
    let mut max_code_length = 0u32;
    for &cl in code_lengths {
        if max_code_length < cl {
            max_code_length = cl;
        }
    }
    const MAX_ALLOWED_CODE_LENGTH: usize = 15;
    if code_lengths.is_empty() || max_code_length > MAX_ALLOWED_CODE_LENGTH as u32 {
        return Err(Error::InvalidHuffmanTree);
    }
    let mut histogram = [0u32; MAX_ALLOWED_CODE_LENGTH + 1];
    for &cl in code_lengths {
        histogram[cl as usize] += 1;
    }
    let mut curr_code = 0u32;
    let mut next_codes = [0u32; MAX_ALLOWED_CODE_LENGTH + 1];
    for cl in 1..next_codes.len() {
        // Go's uint32 arithmetic wraps; a histogram of 2^31 entries is not reachable from an
        // alphabet of at most 2328 symbols, but the shift is spelled out so the port cannot panic.
        curr_code = curr_code.wrapping_add(histogram[cl - 1]) << 1;
        next_codes[cl] = curr_code;
    }
    let mut codes = vec![0u32; code_lengths.len()];
    for (symbol, &cl) in code_lengths.iter().enumerate() {
        if cl > 0 {
            codes[symbol] = next_codes[cl as usize];
            next_codes[cl as usize] += 1;
        }
    }
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_bits_matches_gos_literal() {
        // The first and last rows of huffman.go's table, transcribed.
        let first: [u8; 16] = [
            0x00, 0x80, 0x40, 0xc0, 0x20, 0xa0, 0x60, 0xe0, 0x10, 0x90, 0x50, 0xd0, 0x30, 0xb0,
            0x70, 0xf0,
        ];
        assert_eq!(REVERSE_BITS[..16], first);
        let last: [u8; 16] = [
            0x0f, 0x8f, 0x4f, 0xcf, 0x2f, 0xaf, 0x6f, 0xef, 0x1f, 0x9f, 0x5f, 0xdf, 0x3f, 0xbf,
            0x7f, 0xff,
        ];
        assert_eq!(REVERSE_BITS[240..], last);
        // One row from the middle, where a transcription error would otherwise hide.
        assert_eq!(REVERSE_BITS[0x81], 0x81);
        assert_eq!(REVERSE_BITS[0x82], 0x41);
        assert_eq!(REVERSE_BITS[0xfe], 0x7f);
    }

    #[test]
    fn code_lengths_to_codes_is_canonical() {
        // The classic example: lengths 2,1,3,3 over four symbols.
        assert_eq!(
            code_lengths_to_codes(&[2, 1, 3, 3]).unwrap(),
            vec![2, 0, 6, 7]
        );
        // Zero-length symbols keep code 0 — but they are still counted in `histogram[0]`, which
        // Go folds into the running code before the first real length, so two unused symbols push
        // the two one-bit codes out to 4 and 5 rather than 0 and 1.
        assert_eq!(
            code_lengths_to_codes(&[0, 1, 0, 1]).unwrap(),
            vec![0, 4, 0, 5]
        );
        // With no unused symbols the same two codes are 0 and 1.
        assert_eq!(code_lengths_to_codes(&[1, 1]).unwrap(), vec![0, 1]);
    }

    #[test]
    fn code_lengths_to_codes_rejects_what_go_rejects() {
        assert_eq!(
            code_lengths_to_codes(&[]).err(),
            Some(Error::InvalidHuffmanTree)
        );
        assert_eq!(
            code_lengths_to_codes(&[16]).err(),
            Some(Error::InvalidHuffmanTree)
        );
        assert!(code_lengths_to_codes(&[15]).is_ok());
    }

    #[test]
    fn build_rejects_an_empty_alphabet_and_accepts_a_single_symbol() {
        let mut h = HTree::default();
        assert_eq!(h.build(&[0, 0, 0]).err(), Some(Error::InvalidHuffmanTree));
        let mut h = HTree::default();
        h.build(&[0, 3, 0]).unwrap();
        // The trivial tree is a single leaf: every look-up table entry reports a zero-length code.
        assert_eq!(h.lut[0], 1 << 8 | 1);
        assert_eq!(h.lut[127], 1 << 8 | 1);
    }

    #[test]
    fn build_simple_rejects_a_symbol_outside_the_alphabet() {
        let mut h = HTree::default();
        assert_eq!(
            h.build_simple(2, [0, 300], 256).err(),
            Some(Error::InvalidHuffmanTree)
        );
        let mut h = HTree::default();
        h.build_simple(2, [0, 255], 256).unwrap();
        // Two one-bit codes: entries with bit 0 clear decode to symbol 0, the rest to symbol 255.
        assert_eq!(h.lut[0], 2);
        assert_eq!(h.lut[1], 255 << 8 | 2);
    }
}
