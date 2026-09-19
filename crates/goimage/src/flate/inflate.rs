//! Port of `compress/flate`'s decompressor (compress/flate/inflate.go, go1.26.4).
//!
//! Kept as Go's resumable state machine rather than a one-shot inflate, because the callers
//! observe its pacing: a `Read` runs one step (up to a full 32 KiB window or the end of a block),
//! the error of a failed step is returned together with the bytes it did produce, and the offset
//! in `flate: corrupt input before offset N` counts exactly the input bytes consumed so far.

use super::dict_decoder::DictDecoder;
use crate::goread::{ByteRead, Error, Read, read_full};

/// `maxCodeLen`.
const MAX_CODE_LEN: usize = 16;
/// `maxNumLit`.
const MAX_NUM_LIT: usize = 286;
/// `maxNumDist`.
const MAX_NUM_DIST: usize = 30;
/// `numCodes`.
const NUM_CODES: usize = 19;
/// `maxMatchOffset` (deflate.go) — the window size.
const MAX_MATCH_OFFSET: usize = 1 << 15;
/// `endBlockMarker` (huffman_bit_writer.go).
const END_BLOCK_MARKER: usize = 256;

const HUFFMAN_CHUNK_BITS: usize = 9;
const HUFFMAN_NUM_CHUNKS: usize = 1 << HUFFMAN_CHUNK_BITS;
const HUFFMAN_COUNT_MASK: u32 = 15;
const HUFFMAN_VALUE_SHIFT: u32 = 4;

/// `codeOrder` (inflate.go:320).
const CODE_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Port of `huffmanDecoder` (inflate.go:97): a 9-bit lookup table plus overflow link tables.
#[derive(Clone, Debug)]
struct HuffmanDecoder {
    min: usize,
    chunks: [u32; HUFFMAN_NUM_CHUNKS],
    links: Vec<Vec<u32>>,
    link_mask: u32,
}

impl Default for HuffmanDecoder {
    fn default() -> Self {
        HuffmanDecoder {
            min: 0,
            chunks: [0; HUFFMAN_NUM_CHUNKS],
            links: Vec::new(),
            link_mask: 0,
        }
    }
}

/// `bits.Reverse16(v) >> (16 - n)`.
fn reverse(v: usize, n: usize) -> usize {
    ((v as u16).reverse_bits() as usize) >> (16 - n)
}

impl HuffmanDecoder {
    /// Port of `huffmanDecoder.init` (inflate.go:108). Returns false for an over- or
    /// under-subscribed code, except the degenerate single code of length 1; an all-zero
    /// `lengths` is an empty tree and succeeds.
    fn init(&mut self, lengths: &[usize]) -> bool {
        if self.min != 0 {
            *self = HuffmanDecoder::default();
        }
        let mut count = [0usize; MAX_CODE_LEN];
        let (mut min, mut max) = (0usize, 0usize);
        for &n in lengths {
            if n == 0 {
                continue;
            }
            if min == 0 || n < min {
                min = n;
            }
            if n > max {
                max = n;
            }
            if let Some(c) = count.get_mut(n) {
                *c += 1;
            }
        }
        if max == 0 {
            return true;
        }
        let mut code = 0usize;
        let mut nextcode = [0usize; MAX_CODE_LEN];
        for i in min..=max {
            code <<= 1;
            nextcode[i] = code;
            code += count[i];
        }
        if code != 1 << max && !(code == 1 && max == 1) {
            return false;
        }

        self.min = min;
        if max > HUFFMAN_CHUNK_BITS {
            let num_links = 1usize << (max - HUFFMAN_CHUNK_BITS);
            self.link_mask = (num_links - 1) as u32;
            let link = nextcode[HUFFMAN_CHUNK_BITS + 1] >> 1;
            self.links = vec![Vec::new(); HUFFMAN_NUM_CHUNKS.saturating_sub(link)];
            for j in link..HUFFMAN_NUM_CHUNKS {
                let rev = reverse(j, HUFFMAN_CHUNK_BITS);
                let off = j - link;
                self.chunks[rev] =
                    (off << HUFFMAN_VALUE_SHIFT) as u32 | (HUFFMAN_CHUNK_BITS as u32 + 1);
                self.links[off] = vec![0; num_links];
            }
        }

        for (i, &n) in lengths.iter().enumerate() {
            if n == 0 {
                continue;
            }
            let code = nextcode[n];
            nextcode[n] += 1;
            let chunk = (i << HUFFMAN_VALUE_SHIFT) as u32 | n as u32;
            let rev = reverse(code, n);
            if n <= HUFFMAN_CHUNK_BITS {
                let mut off = rev;
                while off < self.chunks.len() {
                    self.chunks[off] = chunk;
                    off += 1 << n;
                }
            } else {
                let j = rev & (HUFFMAN_NUM_CHUNKS - 1);
                let value = (self.chunks[j] >> HUFFMAN_VALUE_SHIFT) as usize;
                if let Some(linktab) = self.links.get_mut(value) {
                    let mut off = rev >> HUFFMAN_CHUNK_BITS;
                    while off < linktab.len() {
                        linktab[off] = chunk;
                        off += 1 << (n - HUFFMAN_CHUNK_BITS);
                    }
                }
            }
        }
        true
    }
}

/// `fixedHuffmanDecoderInit` (inflate.go:788): RFC 1951 section 3.2.6.
fn fixed_huffman_decoder() -> HuffmanDecoder {
    let mut bits = [0usize; 288];
    for (i, b) in bits.iter_mut().enumerate() {
        *b = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let mut h = HuffmanDecoder::default();
    h.init(&bits);
    h
}

/// Which `step` function runs next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    NextBlock,
    HuffmanBlock,
    CopyData,
}

/// `huffmanBlock`'s resume point (`stateInit` / `stateDict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HState {
    Init,
    Dict,
}

/// Which literal/length table the current block uses (`f.hl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LitTable {
    Fixed,
    Dynamic,
}

/// Port of `decompressor` (inflate.go:246), reading from a `flate.Reader`.
pub struct Decompressor<R> {
    r: R,
    roffset: i64,
    b: u32,
    nb: u32,
    h1: HuffmanDecoder,
    h2: HuffmanDecoder,
    fixed: HuffmanDecoder,
    bits: Vec<usize>,
    codebits: [usize; NUM_CODES],
    dict: DictDecoder,
    step: Step,
    step_state: HState,
    is_final: bool,
    err: Option<Error>,
    to_read: std::ops::Range<usize>,
    hl: LitTable,
    /// `f.hd == nil`: the fixed distance encoding.
    hd_fixed: bool,
    copy_len: usize,
    copy_dist: usize,
}

/// `noEOF`.
fn no_eof(e: Error) -> Error {
    if e == Error::Eof {
        Error::UnexpectedEof
    } else {
        e
    }
}

/// `1<<n - 1` as a `uint32` mask.
fn mask(n: u32) -> u32 {
    1u32.checked_shl(n).unwrap_or(0).wrapping_sub(1)
}

impl<R: ByteRead> Decompressor<R> {
    /// `flate.NewReader` (inflate.go:824) — or `NewReaderDict` with `dict`.
    pub fn new(r: R, dict: Option<&[u8]>) -> Self {
        let mut d = DictDecoder::default();
        d.init(MAX_MATCH_OFFSET, dict);
        Decompressor {
            r,
            roffset: 0,
            b: 0,
            nb: 0,
            h1: HuffmanDecoder::default(),
            h2: HuffmanDecoder::default(),
            fixed: fixed_huffman_decoder(),
            bits: vec![0; MAX_NUM_LIT + MAX_NUM_DIST],
            codebits: [0; NUM_CODES],
            dict: d,
            step: Step::NextBlock,
            step_state: HState::Init,
            is_final: false,
            err: None,
            to_read: 0..0,
            hl: LitTable::Fixed,
            hd_fixed: true,
            copy_len: 0,
            copy_dist: 0,
        }
    }

    /// The underlying reader — zlib reads its checksum from it after the stream.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.r
    }

    fn run_step(&mut self) {
        match self.step {
            Step::NextBlock => self.next_block(),
            Step::HuffmanBlock => self.huffman_block(),
            Step::CopyData => self.copy_data(),
        }
    }

    /// `nextBlock` (inflate.go:278).
    fn next_block(&mut self) {
        while self.nb < 1 + 2 {
            if let Err(e) = self.more_bits() {
                self.err = Some(e);
                return;
            }
        }
        self.is_final = self.b & 1 == 1;
        self.b >>= 1;
        let typ = self.b & 3;
        self.b >>= 2;
        self.nb -= 1 + 2;
        match typ {
            0 => self.data_block(),
            1 => {
                self.hl = LitTable::Fixed;
                self.hd_fixed = true;
                self.huffman_block();
            }
            2 => {
                if let Err(e) = self.read_huffman() {
                    self.err = Some(e);
                    return;
                }
                self.hl = LitTable::Dynamic;
                self.hd_fixed = false;
                self.huffman_block();
            }
            _ => self.err = Some(Error::FlateCorrupt(self.roffset)),
        }
    }

    /// `readHuffman` (inflate.go:322).
    fn read_huffman(&mut self) -> Result<(), Error> {
        while self.nb < 5 + 5 + 4 {
            self.more_bits()?;
        }
        let nlit = (self.b & 0x1f) as usize + 257;
        if nlit > MAX_NUM_LIT {
            return Err(Error::FlateCorrupt(self.roffset));
        }
        self.b >>= 5;
        let ndist = (self.b & 0x1f) as usize + 1;
        if ndist > MAX_NUM_DIST {
            return Err(Error::FlateCorrupt(self.roffset));
        }
        self.b >>= 5;
        let nclen = (self.b & 0xf) as usize + 4;
        self.b >>= 4;
        self.nb -= 5 + 5 + 4;

        for &slot in &CODE_ORDER[..nclen] {
            while self.nb < 3 {
                self.more_bits()?;
            }
            self.codebits[slot] = (self.b & 0x7) as usize;
            self.b >>= 3;
            self.nb -= 3;
        }
        for &slot in &CODE_ORDER[nclen..] {
            self.codebits[slot] = 0;
        }
        let codebits = self.codebits;
        if !self.h1.init(&codebits) {
            return Err(Error::FlateCorrupt(self.roffset));
        }

        let n = nlit + ndist;
        let mut i = 0;
        while i < n {
            let x = self.huff_sym(TableSel::H1)?;
            if x < 16 {
                self.bits[i] = x;
                i += 1;
                continue;
            }
            let (mut rep, nb, b) = match x {
                16 => {
                    if i == 0 {
                        return Err(Error::FlateCorrupt(self.roffset));
                    }
                    (3, 2, self.bits[i - 1])
                }
                17 => (3, 3, 0),
                18 => (11, 7, 0),
                _ => return Err(Error::FlateInternal("unexpected length code")),
            };
            while self.nb < nb {
                self.more_bits()?;
            }
            rep += (self.b & mask(nb)) as usize;
            self.b >>= nb;
            self.nb -= nb;
            if i + rep > n {
                return Err(Error::FlateCorrupt(self.roffset));
            }
            for _ in 0..rep {
                self.bits[i] = b;
                i += 1;
            }
        }

        let (lit, dist) = self.bits.split_at(nlit);
        if !self.h1.init(lit) || !self.h2.init(&dist[..ndist]) {
            return Err(Error::FlateCorrupt(self.roffset));
        }
        if self.h1.min < self.bits[END_BLOCK_MARKER] {
            self.h1.min = self.bits[END_BLOCK_MARKER];
        }
        Ok(())
    }

    /// `huffmanBlock` (inflate.go:431), with its two `goto` labels as a resumable loop.
    fn huffman_block(&mut self) {
        let mut state = self.step_state;
        loop {
            if state == HState::Init {
                // readLiteral
                let lit = match self.hl {
                    LitTable::Fixed => TableSel::Fixed,
                    LitTable::Dynamic => TableSel::H1,
                };
                let v = match self.huff_sym(lit) {
                    Ok(v) => v,
                    Err(e) => {
                        self.err = Some(e);
                        return;
                    }
                };
                let (mut length, n): (usize, u32) = match v {
                    0..=255 => {
                        self.dict.write_byte(v as u8);
                        if self.dict.avail_write() == 0 {
                            self.to_read = self.dict.read_flush();
                            self.step = Step::HuffmanBlock;
                            self.step_state = HState::Init;
                            return;
                        }
                        continue;
                    }
                    256 => {
                        self.finish_block();
                        return;
                    }
                    257..=264 => (v - (257 - 3), 0),
                    265..=268 => (v * 2 - (265 * 2 - 11), 1),
                    269..=272 => (v * 4 - (269 * 4 - 19), 2),
                    273..=276 => (v * 8 - (273 * 8 - 35), 3),
                    277..=280 => (v * 16 - (277 * 16 - 67), 4),
                    281..=284 => (v * 32 - (281 * 32 - 131), 5),
                    285 => (258, 0),
                    _ => {
                        self.err = Some(Error::FlateCorrupt(self.roffset));
                        return;
                    }
                };
                if n > 0 {
                    while self.nb < n {
                        if let Err(e) = self.more_bits() {
                            self.err = Some(e);
                            return;
                        }
                    }
                    length += (self.b & mask(n)) as usize;
                    self.b >>= n;
                    self.nb -= n;
                }

                let mut dist: usize;
                if self.hd_fixed {
                    while self.nb < 5 {
                        if let Err(e) = self.more_bits() {
                            self.err = Some(e);
                            return;
                        }
                    }
                    dist = (((self.b & 0x1f) << 3) as u8).reverse_bits() as usize;
                    self.b >>= 5;
                    self.nb -= 5;
                } else {
                    dist = match self.huff_sym(TableSel::H2) {
                        Ok(d) => d,
                        Err(e) => {
                            self.err = Some(e);
                            return;
                        }
                    };
                }

                if dist < 4 {
                    dist += 1;
                } else if dist < MAX_NUM_DIST {
                    let nb = ((dist - 2) >> 1) as u32;
                    let mut extra = (dist & 1) << nb;
                    while self.nb < nb {
                        if let Err(e) = self.more_bits() {
                            self.err = Some(e);
                            return;
                        }
                    }
                    extra |= (self.b & mask(nb)) as usize;
                    self.b >>= nb;
                    self.nb -= nb;
                    dist = (1 << (nb + 1)) + 1 + extra;
                } else {
                    self.err = Some(Error::FlateCorrupt(self.roffset));
                    return;
                }

                if dist > self.dict.hist_size() {
                    self.err = Some(Error::FlateCorrupt(self.roffset));
                    return;
                }
                self.copy_len = length;
                self.copy_dist = dist;
            }

            // copyHistory
            let mut cnt = self.dict.try_write_copy(self.copy_dist, self.copy_len);
            if cnt == 0 {
                cnt = self.dict.write_copy(self.copy_dist, self.copy_len);
            }
            self.copy_len -= cnt;
            if self.dict.avail_write() == 0 || self.copy_len > 0 {
                self.to_read = self.dict.read_flush();
                self.step = Step::HuffmanBlock;
                self.step_state = HState::Dict;
                return;
            }
            state = HState::Init;
        }
    }

    /// `dataBlock` (inflate.go:590): a stored block.
    fn data_block(&mut self) {
        self.nb = 0;
        self.b = 0;
        let mut buf = [0u8; 4];
        let (nr, err) = read_full(&mut self.r, &mut buf);
        self.roffset += nr as i64;
        if let Some(e) = err {
            self.err = Some(no_eof(e));
            return;
        }
        let n = usize::from(buf[0]) | usize::from(buf[1]) << 8;
        let nn = usize::from(buf[2]) | usize::from(buf[3]) << 8;
        if nn as u16 != !(n as u16) {
            self.err = Some(Error::FlateCorrupt(self.roffset));
            return;
        }
        if n == 0 {
            self.to_read = self.dict.read_flush();
            self.finish_block();
            return;
        }
        self.copy_len = n;
        self.copy_data();
    }

    /// `copyData` (inflate.go:620).
    fn copy_data(&mut self) {
        let range = self.dict.write_range();
        let end = range.start + range.len().min(self.copy_len);
        let (cnt, err) = read_full(&mut self.r, &mut self.dict.hist[range.start..end]);
        self.roffset += cnt as i64;
        self.copy_len -= cnt;
        self.dict.write_mark(cnt);
        if let Some(e) = err {
            self.err = Some(no_eof(e));
            return;
        }
        if self.dict.avail_write() == 0 || self.copy_len > 0 {
            self.to_read = self.dict.read_flush();
            self.step = Step::CopyData;
            return;
        }
        self.finish_block();
    }

    /// `finishBlock` (inflate.go:641).
    fn finish_block(&mut self) {
        if self.is_final {
            if self.dict.avail_read() > 0 {
                self.to_read = self.dict.read_flush();
            }
            self.err = Some(Error::Eof);
        }
        self.step = Step::NextBlock;
    }

    /// `moreBits` (inflate.go:659).
    fn more_bits(&mut self) -> Result<(), Error> {
        let c = self.r.read_byte().map_err(no_eof)?;
        self.roffset += 1;
        self.b |= u32::from(c).checked_shl(self.nb).unwrap_or(0);
        self.nb += 8;
        Ok(())
    }

    /// `huffSym` (inflate.go:671): the next symbol of table `sel`.
    fn huff_sym(&mut self, sel: TableSel) -> Result<usize, Error> {
        let h = match sel {
            TableSel::Fixed => &self.fixed,
            TableSel::H1 => &self.h1,
            TableSel::H2 => &self.h2,
        };
        let mut n = h.min as u32;
        let (mut nb, mut b) = (self.nb, self.b);
        loop {
            while nb < n {
                match self.r.read_byte() {
                    Ok(c) => {
                        self.roffset += 1;
                        b |= u32::from(c) << (nb & 31);
                        nb += 8;
                    }
                    Err(e) => {
                        self.b = b;
                        self.nb = nb;
                        return Err(no_eof(e));
                    }
                }
            }
            let mut chunk = h.chunks[(b as usize) & (HUFFMAN_NUM_CHUNKS - 1)];
            n = chunk & HUFFMAN_COUNT_MASK;
            if n as usize > HUFFMAN_CHUNK_BITS {
                chunk = h
                    .links
                    .get((chunk >> HUFFMAN_VALUE_SHIFT) as usize)
                    .and_then(|t| t.get(((b >> HUFFMAN_CHUNK_BITS) & h.link_mask) as usize))
                    .copied()
                    .unwrap_or(0);
                n = chunk & HUFFMAN_COUNT_MASK;
            }
            if n <= nb {
                if n == 0 {
                    self.b = b;
                    self.nb = nb;
                    let e = Error::FlateCorrupt(self.roffset);
                    self.err = Some(e.clone());
                    return Err(e);
                }
                self.b = b >> (n & 31);
                self.nb = nb - n;
                return Ok((chunk >> HUFFMAN_VALUE_SHIFT) as usize);
            }
        }
    }
}

/// Which Huffman table `huff_sym` reads (`f.hl`, `&f.h1`, `f.hd`).
#[derive(Clone, Copy)]
enum TableSel {
    Fixed,
    H1,
    H2,
}

impl<R: ByteRead> Read for Decompressor<R> {
    /// `decompressor.Read` (inflate.go:307).
    fn read(&mut self, b: &mut [u8]) -> (usize, Option<Error>) {
        loop {
            if !self.to_read.is_empty() {
                let n = b.len().min(self.to_read.len());
                let start = self.to_read.start;
                b[..n].copy_from_slice(&self.dict.hist[start..start + n]);
                self.to_read.start += n;
                if self.to_read.is_empty() {
                    return (n, self.err.clone());
                }
                return (n, None);
            }
            if let Some(e) = &self.err {
                return (0, Some(e.clone()));
            }
            self.run_step();
            if self.err.is_some() && self.to_read.is_empty() {
                self.to_read = self.dict.read_flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::{BytesReader, read_all};

    #[test]
    fn huffman_init_rejects_incomplete_and_accepts_degenerate() {
        let mut h = HuffmanDecoder::default();
        assert!(h.init(&[1, 1]));
        assert!(!h.init(&[1, 1, 1]));
        assert!(!h.init(&[2, 2, 2]));
        assert!(h.init(&[1]), "a single one-bit code is zlib-compatible");
        assert!(!h.init(&[2]));
        assert!(h.init(&[0, 0, 0]), "an empty tree");
        assert_eq!(h.min, 0);
    }

    #[test]
    fn a_long_code_uses_a_link_table() {
        // Lengths 1..=10 plus a second 10: a complete code with two 10-bit codes.
        let lens: Vec<usize> = (1..=10).chain([10]).collect();
        let mut h = HuffmanDecoder::default();
        assert!(h.init(&lens));
        assert_eq!(h.links.len(), 1);
        assert_eq!(h.link_mask, 1);
    }

    #[test]
    fn stored_block_spanning_several_reads() {
        // A stored block of 40000 bytes exceeds the 32 KiB window: two steps.
        let payload: Vec<u8> = (0..40000u32).map(|i| (i * 7) as u8).collect();
        let mut s = vec![0x01, 0x40, 0x9c, 0xbf, 0x63];
        s.extend_from_slice(&payload);
        let mut d = Decompressor::new(BytesReader::new(&s), None);
        let (out, err) = read_all(&mut d);
        assert_eq!(err, None);
        assert_eq!(out, payload);
    }
}

#[cfg(test)]
mod go_parity {
    use crate::goread::{BytesReader, read_all};
    use crate::testsupport::{b64, fixture, sha};

    /// Every stream of the oracle's inflate corpus, through `flate.NewReader` + `io.ReadAll` or
    /// `zlib.NewReader` + `io.ReadAll`: the bytes produced before the error, and its text.
    #[test]
    fn inflate_matches_go_on_every_corpus_stream() {
        let cases = fixture("flate")["inflate"].as_array().unwrap();
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let (out, err) = match c["wrapper"].as_str().unwrap() {
                "zlib" => match crate::zlib::reader::Reader::new(BytesReader::new(&data)) {
                    Ok(mut z) => read_all(&mut z),
                    Err(e) => (Vec::new(), Some(e)),
                },
                _ => read_all(&mut super::Decompressor::new(BytesReader::new(&data), None)),
            };
            assert_eq!(out.len() as u64, c["out_len"].as_u64().unwrap(), "{name}");
            assert_eq!(sha(&out), c["out_sha256"].as_str().unwrap(), "{name}");
            assert_eq!(
                err.map(|e| e.to_string()).as_deref(),
                c["err"].as_str(),
                "{name}"
            );
        }
        assert!(cases.len() > 150, "{}", cases.len());
    }
}
