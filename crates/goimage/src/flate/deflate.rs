//! Port of `compress/flate/deflate.go`: the LZ77 compressor behind `flate.NewWriter`.
//!
//! Levels 0 (stored), 2–9 (hash chains with lazy matching from level 4) and -2 (Huffman only) are
//! ported; -1 is level 6. Level 1 (`BestSpeed`) is Go's separate `deflatefast.go` algorithm,
//! which no Mattermost path reaches (the PNG encoder runs at `BestCompression`), and is refused
//! with [`FlateError::BestSpeedNotPorted`] rather than approximated.

use super::huffman_bit_writer::{HuffmanBitWriter, MAX_STORE_BLOCK_SIZE};
use super::token::{Token, literal_token, match_token};
use crate::sink::Sink;

/// `flate.NoCompression`.
pub const NO_COMPRESSION: i32 = 0;
/// `flate.BestSpeed`.
pub const BEST_SPEED: i32 = 1;
/// `flate.BestCompression`.
pub const BEST_COMPRESSION: i32 = 9;
/// `flate.DefaultCompression`.
pub const DEFAULT_COMPRESSION: i32 = -1;
/// `flate.HuffmanOnly`.
pub const HUFFMAN_ONLY: i32 = -2;

const LOG_WINDOW_SIZE: usize = 15;
const WINDOW_SIZE: usize = 1 << LOG_WINDOW_SIZE;
const WINDOW_MASK: usize = WINDOW_SIZE - 1;
const BASE_MATCH_LENGTH: isize = 3;
const MIN_MATCH_LENGTH: isize = 4;
const MAX_MATCH_LENGTH: isize = 258;
const BASE_MATCH_OFFSET: isize = 1;
const MAX_FLATE_BLOCK_TOKENS: usize = 1 << 14;
const HASH_BITS: u32 = 17;
const HASH_SIZE: usize = 1 << HASH_BITS;
const HASH_MASK: u32 = (1 << HASH_BITS) - 1;
const MAX_HASH_OFFSET: isize = 1 << 24;
const SKIP_NEVER: isize = i32::MAX as isize;
const HASHMUL: u32 = 0x1e35a7bd;
/// `math.MaxInt32`, which `fillDeflate` stores in `blockStart` once a block's start has left the
/// window.
const BLOCK_START_GONE: isize = i32::MAX as isize;

/// Errors from `flate.NewWriter`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlateError {
    /// Go's own refusal of an out-of-range level.
    #[error("flate: invalid compression level {0}: want value in range [-2, 9]")]
    InvalidLevel(i32),
    /// Level 1 is valid in Go but its algorithm (`deflatefast.go`) is not ported.
    #[error("flate: level 1 (BestSpeed) is not ported")]
    BestSpeedNotPorted,
}

/// Port of `compressionLevel` (deflate.go:61).
#[derive(Clone, Copy, Debug)]
struct Level {
    good: isize,
    lazy: isize,
    nice: isize,
    chain: isize,
    fast_skip_hashing: isize,
}

/// `levels` (deflate.go:65).
fn level_params(level: i32) -> Level {
    let (good, lazy, nice, chain, fsh) = match level {
        2 => (4, 0, 16, 8, 5),
        3 => (4, 0, 32, 32, 6),
        4 => (4, 4, 16, 16, SKIP_NEVER),
        5 => (8, 16, 32, 32, SKIP_NEVER),
        6 => (8, 16, 128, 128, SKIP_NEVER),
        7 => (8, 32, 128, 256, SKIP_NEVER),
        8 => (32, 128, 258, 1024, SKIP_NEVER),
        9 => (32, 258, 258, 4096, SKIP_NEVER),
        _ => (0, 0, 0, 0, 0),
    };
    Level {
        good,
        lazy,
        nice,
        chain,
        fast_skip_hashing: fsh,
    }
}

/// Go picks `fill`/`step` function pointers per level; this is that choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Store,
    StoreHuff,
    Deflate,
}

/// `hash4` (deflate.go:283).
fn hash4(b: &[u8]) -> u32 {
    ((u32::from(b[3]) | u32::from(b[2]) << 8 | u32::from(b[1]) << 16 | u32::from(b[0]) << 24)
        .wrapping_mul(HASHMUL))
        >> (32 - HASH_BITS)
}

/// `matchLen` (deflate.go:306).
fn match_len(a: &[u8], b: &[u8], max: usize) -> usize {
    a[..max]
        .iter()
        .zip(&b[..max])
        .position(|(x, y)| x != y)
        .unwrap_or(max)
}

/// Port of `compressor` (deflate.go:77).
struct Compressor<W: Sink> {
    lvl: Level,
    mode: Mode,
    w: HuffmanBitWriter<W>,
    index: isize,
    window: Vec<u8>,
    window_end: isize,
    block_start: isize,
    byte_available: bool,
    sync: bool,
    tokens: Vec<Token>,
    length: isize,
    offset: isize,
    max_insert_index: isize,
    chain_head: isize,
    hash_head: Vec<u32>,
    hash_prev: Vec<u32>,
    hash_offset: isize,
    closed: bool,
}

impl<W: Sink> Compressor<W> {
    /// `compressor.init` (deflate.go:586).
    fn new(w: W, level: i32) -> Result<Self, FlateError> {
        let mut c = Compressor {
            lvl: level_params(0),
            mode: Mode::Store,
            w: HuffmanBitWriter::new(w),
            index: 0,
            window: Vec::new(),
            window_end: 0,
            block_start: 0,
            byte_available: false,
            sync: false,
            tokens: Vec::new(),
            length: 0,
            offset: 0,
            max_insert_index: 0,
            chain_head: 0,
            hash_head: Vec::new(),
            hash_prev: Vec::new(),
            hash_offset: 0,
            closed: false,
        };
        match level {
            NO_COMPRESSION => {
                c.window = vec![0; MAX_STORE_BLOCK_SIZE];
                c.mode = Mode::Store;
            }
            HUFFMAN_ONLY => {
                c.window = vec![0; MAX_STORE_BLOCK_SIZE];
                c.mode = Mode::StoreHuff;
            }
            BEST_SPEED => return Err(FlateError::BestSpeedNotPorted),
            -1 | 2..=9 => {
                let level = if level == DEFAULT_COMPRESSION {
                    6
                } else {
                    level
                };
                c.lvl = level_params(level);
                c.mode = Mode::Deflate;
                // initDeflate (deflate.go:352).
                c.window = vec![0; 2 * WINDOW_SIZE];
                c.hash_offset = 1;
                c.tokens = Vec::with_capacity(MAX_FLATE_BLOCK_TOKENS + 1);
                c.length = MIN_MATCH_LENGTH - 1;
                c.offset = 0;
                c.byte_available = false;
                c.index = 0;
                c.chain_head = -1;
                c.hash_head = vec![0; HASH_SIZE];
                c.hash_prev = vec![0; WINDOW_SIZE];
            }
            _ => return Err(FlateError::InvalidLevel(level)),
        }
        Ok(c)
    }

    fn fill(&mut self, b: &[u8]) -> usize {
        match self.mode {
            Mode::Deflate => self.fill_deflate(b),
            Mode::Store | Mode::StoreHuff => self.fill_store(b),
        }
    }

    fn step(&mut self) {
        match self.mode {
            Mode::Deflate => self.deflate(),
            Mode::Store => self.store(),
            Mode::StoreHuff => self.store_huff(),
        }
    }

    /// `fillDeflate` (deflate.go:117).
    fn fill_deflate(&mut self, b: &[u8]) -> usize {
        if self.index >= (2 * WINDOW_SIZE) as isize - (MIN_MATCH_LENGTH + MAX_MATCH_LENGTH) {
            self.window.copy_within(WINDOW_SIZE..2 * WINDOW_SIZE, 0);
            self.index -= WINDOW_SIZE as isize;
            self.window_end -= WINDOW_SIZE as isize;
            if self.block_start >= WINDOW_SIZE as isize {
                self.block_start -= WINDOW_SIZE as isize;
            } else {
                self.block_start = BLOCK_START_GONE;
            }
            self.hash_offset += WINDOW_SIZE as isize;
            if self.hash_offset > MAX_HASH_OFFSET {
                let delta = self.hash_offset - 1;
                self.hash_offset -= delta;
                self.chain_head -= delta;
                for v in self.hash_prev.iter_mut().chain(self.hash_head.iter_mut()) {
                    *v = if *v as isize > delta {
                        (*v as isize - delta) as u32
                    } else {
                        0
                    };
                }
            }
        }
        self.fill_store(b)
    }

    /// `fillStore` (deflate.go:540) — also the tail of `fillDeflate`.
    fn fill_store(&mut self, b: &[u8]) -> usize {
        let end = self.window_end as usize;
        let n = b.len().min(self.window.len() - end);
        self.window[end..end + n].copy_from_slice(&b[..n]);
        self.window_end += n as isize;
        n
    }

    /// `writeBlock` (deflate.go:164).
    fn write_block(&mut self, index: isize) {
        if index > 0 {
            let window = if self.block_start <= index {
                Some(&self.window[self.block_start as usize..index as usize])
            } else {
                None
            };
            self.block_start = index;
            self.w.write_block(&mut self.tokens, false, window);
        }
    }

    /// `writeStoredBlock` (deflate.go:269).
    fn write_stored_block(&mut self, len: usize) {
        self.w.write_stored_header(len, false);
        self.w.write_bytes(&self.window[..len]);
    }

    /// `hashPrev`/`hashHead` insertion of the string at `index`.
    fn insert_hash(&mut self, index: isize) {
        let i = index as usize;
        let hash = hash4(&self.window[i..i + MIN_MATCH_LENGTH as usize]);
        let hh = &mut self.hash_head[(hash & HASH_MASK) as usize];
        self.hash_prev[i & WINDOW_MASK] = *hh;
        *hh = (index + self.hash_offset) as u32;
    }

    /// `findMatch` (deflate.go:223).
    fn find_match(
        &self,
        pos: isize,
        prev_head: isize,
        prev_length: isize,
        lookahead: isize,
    ) -> (isize, isize, bool) {
        let min_match_look = MAX_MATCH_LENGTH.min(lookahead);
        let win = &self.window[..(pos + min_match_look) as usize];
        let nice = (win.len() as isize - pos).min(self.lvl.nice);
        let mut tries = self.lvl.chain;
        let mut length = prev_length;
        if length >= self.lvl.good {
            tries >>= 2;
        }
        let mut offset = 0;
        let mut ok = false;
        let mut w_end = win[(pos + length) as usize];
        let w_pos = &win[pos as usize..];
        let min_index = pos - WINDOW_SIZE as isize;

        let mut i = prev_head;
        while tries > 0 {
            if w_end == win[(i + length) as usize] {
                let n = match_len(&win[i as usize..], w_pos, min_match_look as usize) as isize;
                if n > length && (n > MIN_MATCH_LENGTH || pos - i <= 4096) {
                    length = n;
                    offset = pos - i;
                    ok = true;
                    if n >= nice {
                        break;
                    }
                    w_end = win[(pos + n) as usize];
                }
            }
            if i == min_index {
                break;
            }
            i = self.hash_prev[i as usize & WINDOW_MASK] as isize - self.hash_offset;
            if i < min_index || i < 0 {
                break;
            }
            tries -= 1;
        }
        (length, offset, ok)
    }

    /// Port of `compressor.deflate` (deflate.go:363).
    fn deflate(&mut self) {
        if self.window_end - self.index < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH && !self.sync {
            return;
        }
        self.max_insert_index = self.window_end - (MIN_MATCH_LENGTH - 1);
        let fsh = self.lvl.fast_skip_hashing;

        loop {
            let lookahead = self.window_end - self.index;
            if lookahead < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH {
                if !self.sync {
                    break;
                }
                if lookahead == 0 {
                    if self.byte_available {
                        let lit = self.window[(self.index - 1) as usize];
                        self.tokens.push(literal_token(u32::from(lit)));
                        self.byte_available = false;
                    }
                    if !self.tokens.is_empty() {
                        self.write_block(self.index);
                        self.tokens.clear();
                    }
                    break;
                }
            }
            if self.index < self.max_insert_index {
                let i = self.index as usize;
                let hash = hash4(&self.window[i..i + MIN_MATCH_LENGTH as usize]);
                let hh = &mut self.hash_head[(hash & HASH_MASK) as usize];
                self.chain_head = *hh as isize;
                self.hash_prev[i & WINDOW_MASK] = self.chain_head as u32;
                *hh = (self.index + self.hash_offset) as u32;
            }
            let prev_length = self.length;
            let prev_offset = self.offset;
            self.length = MIN_MATCH_LENGTH - 1;
            self.offset = 0;
            let min_index = (self.index - WINDOW_SIZE as isize).max(0);

            if self.chain_head - self.hash_offset >= min_index
                && (fsh != SKIP_NEVER && lookahead > MIN_MATCH_LENGTH - 1
                    || fsh == SKIP_NEVER && lookahead > prev_length && prev_length < self.lvl.lazy)
            {
                let (new_length, new_offset, ok) = self.find_match(
                    self.index,
                    self.chain_head - self.hash_offset,
                    MIN_MATCH_LENGTH - 1,
                    lookahead,
                );
                if ok {
                    self.length = new_length;
                    self.offset = new_offset;
                }
            }
            if fsh != SKIP_NEVER && self.length >= MIN_MATCH_LENGTH
                || fsh == SKIP_NEVER
                    && prev_length >= MIN_MATCH_LENGTH
                    && self.length <= prev_length
            {
                if fsh != SKIP_NEVER {
                    self.tokens.push(match_token(
                        (self.length - BASE_MATCH_LENGTH) as u32,
                        (self.offset - BASE_MATCH_OFFSET) as u32,
                    ));
                } else {
                    self.tokens.push(match_token(
                        (prev_length - BASE_MATCH_LENGTH) as u32,
                        (prev_offset - BASE_MATCH_OFFSET) as u32,
                    ));
                }
                if self.length <= fsh {
                    let new_index = if fsh != SKIP_NEVER {
                        self.index + self.length
                    } else {
                        self.index + prev_length - 1
                    };
                    let mut index = self.index + 1;
                    while index < new_index {
                        if index < self.max_insert_index {
                            self.insert_hash(index);
                        }
                        index += 1;
                    }
                    self.index = index;
                    if fsh == SKIP_NEVER {
                        self.byte_available = false;
                        self.length = MIN_MATCH_LENGTH - 1;
                    }
                } else {
                    self.index += self.length;
                }
                if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                    self.write_block(self.index);
                    self.tokens.clear();
                }
            } else {
                if fsh != SKIP_NEVER || self.byte_available {
                    let i = if fsh != SKIP_NEVER {
                        self.index
                    } else {
                        self.index - 1
                    };
                    self.tokens
                        .push(literal_token(u32::from(self.window[i as usize])));
                    if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                        self.write_block(i + 1);
                        self.tokens.clear();
                    }
                }
                self.index += 1;
                if fsh == SKIP_NEVER {
                    self.byte_available = true;
                }
            }
        }
    }

    /// `store` (deflate.go:546).
    fn store(&mut self) {
        if self.window_end > 0 && (self.window_end as usize == MAX_STORE_BLOCK_SIZE || self.sync) {
            self.write_stored_block(self.window_end as usize);
            self.window_end = 0;
        }
    }

    /// `storeHuff` (deflate.go:556).
    fn store_huff(&mut self) {
        if (self.window_end as usize) < self.window.len() && !self.sync || self.window_end == 0 {
            return;
        }
        let end = self.window_end as usize;
        self.w.write_block_huff(false, &self.window[..end]);
        self.window_end = 0;
    }

    /// `compressor.write` (deflate.go:565).
    fn write(&mut self, mut b: &[u8]) {
        while !b.is_empty() {
            self.step();
            let n = self.fill(b);
            b = &b[n..];
        }
    }

    /// `compressor.close` (deflate.go:660).
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.sync = true;
        self.step();
        self.w.write_stored_header(0, true);
        self.w.flush();
        self.closed = true;
    }
}

/// Port of `flate.Writer` (deflate.go:726): `NewWriter`, `Write`, `Close`.
pub struct Writer<W: Sink> {
    d: Compressor<W>,
}

impl<W: Sink> Writer<W> {
    /// Port of `flate.NewWriter` (deflate.go:692).
    pub fn new(w: W, level: i32) -> Result<Self, FlateError> {
        Ok(Writer {
            d: Compressor::new(w, level)?,
        })
    }

    /// `Writer.Write`.
    pub fn write(&mut self, data: &[u8]) {
        self.d.write(data);
    }

    /// `Writer.Close`: flush everything and write the final empty stored block. A second call is
    /// a no-op, as in Go.
    pub fn close(&mut self) {
        self.d.close();
    }

    /// The underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.d.w.writer
    }

    /// Consume the writer, returning the underlying one. Does not close.
    pub fn into_inner(self) -> W {
        self.d.w.writer
    }
}

impl<W: Sink> Sink for Writer<W> {
    fn write(&mut self, p: &[u8]) {
        self.d.write(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_rejects_levels_outside_minus_two_to_nine() {
        for level in [-3, 10, 100] {
            assert_eq!(
                Writer::new(Vec::new(), level).err().map(|e| e.to_string()),
                Some(format!(
                    "flate: invalid compression level {level}: want value in range [-2, 9]"
                ))
            );
        }
        assert_eq!(
            Writer::new(Vec::new(), 1).err(),
            Some(FlateError::BestSpeedNotPorted)
        );
    }

    #[test]
    fn empty_input_is_one_final_empty_stored_block() {
        // Go: flate.NewWriter(&b, 9); w.Close() writes 01 00 00 ff ff.
        for level in [0, 2, 9, -1, -2] {
            let mut w = Writer::new(Vec::new(), level).unwrap();
            w.close();
            w.close();
            assert_eq!(w.into_inner(), vec![1, 0, 0, 0xff, 0xff], "level {level}");
        }
    }
}

#[cfg(test)]
mod go_parity {
    use super::Writer;
    use crate::testsupport::{assert_encoded, fixture, hash4, sample, sha};
    use crate::zlib::Writer as ZlibWriter;

    const WORDS: [&str; 16] = [
        "the",
        "channel",
        "mattermost",
        "post",
        "a",
        "of",
        "image",
        "preview",
        "thumbnail",
        "and",
        "user",
        "team",
        "to",
        "in",
        "file",
        "is",
    ];

    /// Mirror of `streamSpec.build` (reference/dump/behaviour_imaging_codec.go).
    fn build_stream(spec: &serde_json::Value) -> Vec<u8> {
        let kind = spec["kind"].as_str().unwrap();
        let len = spec["len"].as_u64().unwrap() as usize;
        let seed = spec["seed"].as_u64().unwrap();
        let mut out = Vec::with_capacity(len + 16);
        match kind {
            "zeros" => out.resize(len, 0),
            "noise" => {
                for i in 0..len {
                    out.push((hash4(seed, i as i64, 0, 0) >> 56) as u8);
                }
            }
            "small" => {
                for i in 0..len {
                    out.push(b'a' + (hash4(seed, i as i64, 0, 0) >> 62) as u8);
                }
            }
            "text" => {
                let mut n = 0;
                while out.len() < len {
                    out.extend_from_slice(WORDS[(hash4(seed, n, 0, 0) >> 60) as usize].as_bytes());
                    out.push(b' ');
                    n += 1;
                }
                out.truncate(len);
            }
            "runs" => {
                let mut n = 0;
                while out.len() < len {
                    let b = (hash4(seed, n, 0, 0) >> 56) as u8;
                    let run = 1 + (hash4(seed, n, 1, 0) >> 58) as usize;
                    out.extend(std::iter::repeat_n(b, run));
                    n += 1;
                }
                out.truncate(len);
            }
            "records" => {
                for i in 0..len as i64 {
                    out.push(if i % 5 == 4 {
                        (hash4(seed, i, 0, 0) >> 56) as u8
                    } else {
                        b"mmrs"[(i % 5) as usize]
                    });
                }
            }
            "rows" => {
                for i in 0..len as i64 {
                    out.push(sample(
                        "blocks",
                        seed,
                        (i / 3) % 97,
                        i / 291,
                        i % 3,
                        97,
                        1000,
                    ));
                }
            }
            other => panic!("unknown stream kind {other}"),
        }
        out
    }

    fn feed(input: &[u8], chunk: usize, mut write: impl FnMut(&[u8])) {
        let mut rest = input;
        while !rest.is_empty() {
            let n = if chunk > 0 && chunk < rest.len() {
                chunk
            } else {
                rest.len()
            };
            write(&rest[..n]);
            rest = &rest[n..];
        }
    }

    /// Every `deflate` case of behaviour_imaging_flate.json: six input shapes from 0 bytes to
    /// 1 MiB, levels 0, 2–9 and -1, chunked writes, and the zlib wrapper.
    #[test]
    fn every_deflate_case_matches_go_byte_for_byte() {
        let cases = fixture("flate")["deflate"].as_array().unwrap();
        for c in cases {
            let input = build_stream(&c["input"]);
            assert_eq!(
                sha(&input),
                c["input_sha256"].as_str().unwrap(),
                "{}",
                c["input"]
            );
            let level = c["level"].as_i64().unwrap() as i32;
            let chunk = c["chunk"].as_u64().unwrap() as usize;
            let out = if c["wrapper"] == "zlib" {
                let mut w = ZlibWriter::new(Vec::new(), level).unwrap();
                feed(&input, chunk, |p| w.write(p));
                w.close();
                w.into_inner()
            } else {
                let mut w = Writer::new(Vec::new(), level).unwrap();
                feed(&input, chunk, |p| w.write(p));
                w.close();
                w.into_inner()
            };
            let name = format!(
                "{} level {level} chunk {chunk} {}",
                c["input"], c["wrapper"]
            );
            assert_encoded(&name, &out, &c["output"]);
        }
        assert!(cases.len() > 250, "{}", cases.len());
    }
}
