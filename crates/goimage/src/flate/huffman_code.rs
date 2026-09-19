//! Port of `compress/flate/huffman_code.go`: length-limited Huffman code construction.
//!
//! Go sorts with `sort.Sort`, which is not stable; both orderings used here (`byFreq`, which
//! breaks frequency ties by literal, and `byLiteral`) are total orders over distinct literals, so
//! any correct sort yields Go's permutation.

use std::sync::OnceLock;

/// `maxNumLit` (inflate.go:23): literal/length alphabet size.
pub(crate) const MAX_NUM_LIT: usize = 286;
const MAX_BITS_LIMIT: usize = 16;

/// Port of `hcode` (huffman_code.go:15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HCode {
    pub code: u16,
    pub len: u16,
}

#[derive(Clone, Copy, Debug, Default)]
struct LiteralNode {
    literal: u16,
    freq: i32,
}

#[derive(Clone, Copy, Debug, Default)]
struct LevelInfo {
    level: i32,
    last_freq: i32,
    next_char_freq: i32,
    next_pair_freq: i32,
    needed: i32,
}

/// Port of `huffmanEncoder` (huffman_code.go:19).
#[derive(Clone, Debug)]
pub(crate) struct HuffmanEncoder {
    pub codes: Vec<HCode>,
    freqcache: Vec<LiteralNode>,
    bit_count: [i32; 17],
}

fn max_node() -> LiteralNode {
    LiteralNode {
        literal: u16::MAX,
        freq: i32::MAX,
    }
}

/// `reverseBits` (huffman_code.go:343).
pub(crate) fn reverse_bits(number: u16, bit_length: u8) -> u16 {
    (number << (16 - u32::from(bit_length))).reverse_bits()
}

impl HuffmanEncoder {
    /// `newHuffmanEncoder`.
    pub(crate) fn new(size: usize) -> HuffmanEncoder {
        HuffmanEncoder {
            codes: vec![HCode::default(); size],
            freqcache: Vec::new(),
            bit_count: [0; 17],
        }
    }

    /// `bitLength` (huffman_code.go:103).
    pub(crate) fn bit_length(&self, freq: &[i32]) -> i64 {
        let mut total = 0i64;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                total += i64::from(f) * i64::from(self.codes[i].len);
            }
        }
        total
    }

    /// `bitCounts` (huffman_code.go:130). `list` holds the `n` live nodes followed by one spare
    /// slot for the sentinel.
    fn bit_counts(&mut self, list: &mut [LiteralNode], n: usize, mut max_bits: i32) -> Vec<i32> {
        let n32 = n as i32;
        list[n] = max_node();
        if max_bits > n32 - 1 {
            max_bits = n32 - 1;
        }
        let mut levels = [LevelInfo::default(); MAX_BITS_LIMIT];
        let mut leaf_counts = [[0i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT];
        for level in 1..=max_bits as usize {
            levels[level] = LevelInfo {
                level: level as i32,
                last_freq: list[1].freq,
                next_char_freq: list[2].freq,
                next_pair_freq: list[0].freq + list[1].freq,
                needed: 0,
            };
            leaf_counts[level][level] = 2;
            if level == 1 {
                levels[level].next_pair_freq = i32::MAX;
            }
        }
        let mb = max_bits as usize;
        levels[mb].needed = 2 * n32 - 4;

        let mut level = mb;
        loop {
            let l = levels[level];
            if l.next_pair_freq == i32::MAX && l.next_char_freq == i32::MAX {
                levels[level].needed = 0;
                levels[level + 1].next_pair_freq = i32::MAX;
                level += 1;
                continue;
            }
            let prev_freq = l.last_freq;
            if l.next_char_freq < l.next_pair_freq {
                let nn = leaf_counts[level][level] + 1;
                levels[level].last_freq = l.next_char_freq;
                leaf_counts[level][level] = nn;
                levels[level].next_char_freq = list[nn as usize].freq;
            } else {
                levels[level].last_freq = l.next_pair_freq;
                let (lower, upper) = leaf_counts.split_at_mut(level);
                upper[0][..level].copy_from_slice(&lower[level - 1][..level]);
                levels[(l.level - 1) as usize].needed = 2;
            }
            levels[level].needed -= 1;
            if levels[level].needed == 0 {
                if levels[level].level as usize == mb {
                    break;
                }
                let lvl = levels[level].level as usize;
                levels[lvl + 1].next_pair_freq = prev_freq + levels[level].last_freq;
                level += 1;
            } else {
                while levels[level - 1].needed > 0 {
                    level -= 1;
                }
            }
        }

        // Go panics when leafCounts[maxBits][maxBits] != n; the construction guarantees it.
        debug_assert_eq!(leaf_counts[mb][mb], n32);

        let mut bits = 1usize;
        let counts = leaf_counts[mb];
        let mut level = mb;
        while level > 0 {
            self.bit_count[bits] = counts[level] - counts[level - 1];
            bits += 1;
            level -= 1;
        }
        self.bit_count[..=mb].to_vec()
    }

    /// `assignEncodingAndSize` (huffman_code.go:242).
    fn assign_encoding_and_size(&mut self, bit_count: &[i32], list: &mut [LiteralNode]) {
        let mut code: u16 = 0;
        let mut end = list.len();
        for (n, &bits) in bit_count.iter().enumerate() {
            code = code.wrapping_shl(1);
            if n == 0 || bits == 0 {
                continue;
            }
            let chunk = &mut list[end - bits as usize..end];
            chunk.sort_unstable_by_key(|node| node.literal);
            for node in chunk.iter() {
                self.codes[node.literal as usize] = HCode {
                    code: reverse_bits(code, n as u8),
                    len: n as u16,
                };
                code = code.wrapping_add(1);
            }
            end -= bits as usize;
        }
    }

    /// Port of `huffmanEncoder.generate` (huffman_code.go:270).
    pub(crate) fn generate(&mut self, freq: &[i32], max_bits: i32) {
        let mut list = std::mem::take(&mut self.freqcache);
        if list.len() < MAX_NUM_LIT + 1 {
            list = vec![LiteralNode::default(); MAX_NUM_LIT + 1];
        }
        let mut count = 0;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                list[count] = LiteralNode {
                    literal: i as u16,
                    freq: f,
                };
                count += 1;
            } else {
                self.codes[i].len = 0;
            }
        }
        if count <= 2 {
            for (i, node) in list[..count].iter().enumerate() {
                self.codes[node.literal as usize] = HCode {
                    code: i as u16,
                    len: 1,
                };
            }
            self.freqcache = list;
            return;
        }
        list[..count].sort_unstable_by(|a, b| a.freq.cmp(&b.freq).then(a.literal.cmp(&b.literal)));
        let bit_count = self.bit_counts(&mut list, count, max_bits);
        self.assign_encoding_and_size(&bit_count, &mut list[..count]);
        self.freqcache = list;
    }
}

/// `generateFixedLiteralEncoding` (huffman_code.go:67).
fn generate_fixed_literal_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(MAX_NUM_LIT);
    for ch in 0..MAX_NUM_LIT as u16 {
        let (bits, size) = match ch {
            0..=143 => (ch + 48, 8),
            144..=255 => (ch + 400 - 144, 9),
            256..=279 => (ch - 256, 7),
            _ => (ch + 192 - 280, 8),
        };
        h.codes[ch as usize] = HCode {
            code: reverse_bits(bits, size as u8),
            len: size,
        };
    }
    h
}

/// `generateFixedOffsetEncoding` (huffman_code.go:94).
fn generate_fixed_offset_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(30);
    for ch in 0..30u16 {
        h.codes[ch as usize] = HCode {
            code: reverse_bits(ch, 5),
            len: 5,
        };
    }
    h
}

/// `fixedLiteralEncoding`.
pub(crate) fn fixed_literal_encoding() -> &'static HuffmanEncoder {
    static E: OnceLock<HuffmanEncoder> = OnceLock::new();
    E.get_or_init(generate_fixed_literal_encoding)
}

/// `fixedOffsetEncoding`.
pub(crate) fn fixed_offset_encoding() -> &'static HuffmanEncoder {
    static E: OnceLock<HuffmanEncoder> = OnceLock::new();
    E.get_or_init(generate_fixed_offset_encoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_bits_matches_go() {
        assert_eq!(reverse_bits(0b1, 1), 0b1);
        assert_eq!(reverse_bits(0b110, 3), 0b011);
        assert_eq!(reverse_bits(0x30, 8), 0x0c);
    }

    #[test]
    fn two_or_fewer_literals_get_one_bit_each() {
        let mut h = HuffmanEncoder::new(5);
        h.generate(&[0, 7, 0, 3, 0], 15);
        assert_eq!(h.codes[1], HCode { code: 0, len: 1 });
        assert_eq!(h.codes[3], HCode { code: 1, len: 1 });
    }

    #[test]
    fn lengths_respect_the_limit_and_form_a_complete_code() {
        // Fibonacci frequencies force depth past 7 unless the limit bites.
        let mut freq = vec![0i32; 19];
        let (mut a, mut b) = (1, 1);
        for f in freq.iter_mut() {
            *f = a;
            (a, b) = (b, a + b);
        }
        let mut h = HuffmanEncoder::new(19);
        h.generate(&freq, 7);
        let kraft: f64 = h.codes.iter().map(|c| 2f64.powi(-i32::from(c.len))).sum();
        assert!(h.codes.iter().all(|c| (1..=7).contains(&c.len)));
        assert!((kraft - 1.0).abs() < 1e-12, "{kraft}");
    }
}
