//! Port of `golang.org/x/image/vp8l` (decode.go, huffman.go, transform.go): the VP8L lossless
//! image format, which always decodes to an `*image.NRGBA`.
//!
//! The bit-stream is read least-significant-bit first through [`BitReader`], and every symbol comes
//! from one of five canonical Huffman trees per tile group (green-or-length, red, blue, alpha,
//! distance). Section numbers in the comments are the VP8L specification's, as in the Go source.

pub mod huffman;
pub mod transform;

use huffman::HTree;
use transform::{
    N_TRANSFORM_TYPES, TRANSFORM_TYPE_COLOR_INDEXING, TRANSFORM_TYPE_CROSS_COLOR,
    TRANSFORM_TYPE_PREDICTOR, TRANSFORM_TYPE_SUBTRACT_GREEN, Transform, inverse_transform, n_tiles,
};

use crate::goread::{self, ByteRead};
use crate::image::{Image, Pixels, Rect};

/// Every error value the VP8L decoder can produce, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `errInvalidCodeLengths`.
    #[error("vp8l: invalid code lengths")]
    InvalidCodeLengths,
    /// `errInvalidHuffmanTree`.
    #[error("vp8l: invalid Huffman tree")]
    InvalidHuffmanTree,
    /// decode.go:333.
    #[error("vp8l: invalid color cache parameters")]
    InvalidColorCacheParameters,
    /// decode.go:412.
    #[error("vp8l: invalid LZ77 parameters")]
    InvalidLz77Parameters,
    /// decode.go:433. Unreachable in practice: the green tree's alphabet is exactly
    /// `256 + 24 + 1<<ccBits` symbols, so a cache index is always inside the cache.
    #[error("vp8l: invalid color cache index")]
    InvalidColorCacheIndex,
    /// decode.go:494.
    #[error("vp8l: invalid header")]
    InvalidHeader,
    /// decode.go:511.
    #[error("vp8l: invalid version")]
    InvalidVersion,
    /// decode.go:556.
    #[error("vp8l: repeated transform")]
    RepeatedTransform,
    /// An error from the underlying reader.
    #[error(transparent)]
    Io(#[from] goread::Error),
}

/// `colorCacheMultiplier`: the multiplier of the colour cache's hash function (section 4.2.3).
const COLOR_CACHE_MULTIPLIER: u32 = 0x1e35_a7bd;

/// `distanceMapTable`: the look-up table for [`distance_map`].
const DISTANCE_MAP_TABLE: [u8; 120] = [
    0x18, 0x07, 0x17, 0x19, 0x28, 0x06, 0x27, 0x29, 0x16, 0x1a, //
    0x26, 0x2a, 0x38, 0x05, 0x37, 0x39, 0x15, 0x1b, 0x36, 0x3a, //
    0x25, 0x2b, 0x48, 0x04, 0x47, 0x49, 0x14, 0x1c, 0x35, 0x3b, //
    0x46, 0x4a, 0x24, 0x2c, 0x58, 0x45, 0x4b, 0x34, 0x3c, 0x03, //
    0x57, 0x59, 0x13, 0x1d, 0x56, 0x5a, 0x23, 0x2d, 0x44, 0x4c, //
    0x55, 0x5b, 0x33, 0x3d, 0x68, 0x02, 0x67, 0x69, 0x12, 0x1e, //
    0x66, 0x6a, 0x22, 0x2e, 0x54, 0x5c, 0x43, 0x4d, 0x65, 0x6b, //
    0x32, 0x3e, 0x78, 0x01, 0x77, 0x79, 0x53, 0x5d, 0x11, 0x1f, //
    0x64, 0x6c, 0x42, 0x4e, 0x76, 0x7a, 0x21, 0x2f, 0x75, 0x7b, //
    0x31, 0x3f, 0x63, 0x6d, 0x52, 0x5e, 0x00, 0x74, 0x7c, 0x41, //
    0x4f, 0x10, 0x20, 0x62, 0x6e, 0x30, 0x73, 0x7d, 0x51, 0x5f, //
    0x40, 0x72, 0x7e, 0x61, 0x6f, 0x50, 0x71, 0x7f, 0x60, 0x70, //
];

/// `distanceMap`: a LZ77 backwards-reference distance as a two-dimensional pixel offset
/// (section 4.2.2).
fn distance_map(w: i32, code: u32) -> i32 {
    if code as i32 > DISTANCE_MAP_TABLE.len() as i32 {
        return code as i32 - DISTANCE_MAP_TABLE.len() as i32;
    }
    // `code` is at least 1 — `lz77Param` never returns zero — so `code - 1` is a real index and Go
    // never reaches its own out-of-range panic here. The saturation keeps that true in Rust.
    let dist_code = i32::from(DISTANCE_MAP_TABLE[(code.max(1) - 1) as usize]);
    let y_offset = dist_code >> 4;
    let x_offset = 8 - (dist_code & 0xf);
    let d = y_offset * w + x_offset;
    if d >= 1 { d } else { 1 }
}

/// `decoder`: the bit-stream of a VP8L image, read least-significant-bit first.
pub struct BitReader<'a> {
    r: &'a mut dyn ByteRead,
    pub(crate) bits: u32,
    pub(crate) n_bits: u32,
}

impl BitReader<'_> {
    pub(crate) fn read_byte(&mut self) -> Result<u8, goread::Error> {
        self.r.read_byte()
    }

    /// `decoder.read`: the next `n` bits. `n` never exceeds 18 here, so no shift reaches 32.
    fn read(&mut self, n: u32) -> Result<u32, Error> {
        while self.n_bits < n {
            let c = match self.r.read_byte() {
                Ok(c) => c,
                Err(goread::Error::Eof) => return Err(Error::Io(goread::Error::UnexpectedEof)),
                Err(e) => return Err(Error::Io(e)),
            };
            self.bits |= u32::from(c) << self.n_bits;
            self.n_bits += 8;
        }
        let u = self.bits & ((1u32 << n) - 1);
        self.bits >>= n;
        self.n_bits -= n;
        Ok(u)
    }

    /// `decoder.decodeTransform`: the next transform and the width after transformation
    /// (section 3).
    fn decode_transform(&mut self, mut w: i32, h: i32) -> Result<(Transform, i32), Error> {
        let mut t = Transform {
            old_width: w,
            ..Transform::default()
        };
        t.transform_type = self.read(2)?;
        match t.transform_type {
            TRANSFORM_TYPE_PREDICTOR | TRANSFORM_TYPE_CROSS_COLOR => {
                t.bits = self.read(3)? + 2;
                t.pix = self.decode_pix(n_tiles(w, t.bits), n_tiles(h, t.bits), 0, false)?;
            }
            TRANSFORM_TYPE_SUBTRACT_GREEN => {
                // No-op.
            }
            TRANSFORM_TYPE_COLOR_INDEXING => {
                let n_colors = self.read(8)? + 1;
                t.bits = match n_colors {
                    0..=2 => 3,
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                w = n_tiles(w, t.bits);
                let mut pix = self.decode_pix(n_colors as i32, 1, 4 * 256, false)?;
                let mut p = 4;
                while p < pix.len() {
                    for k in 0..4 {
                        pix[p + k] = pix[p + k].wrapping_add(pix[p + k - 4]);
                    }
                    p += 4;
                }
                // The spec says that "if the index is equal or larger than color_table_size, the
                // argb color value should be set to 0x00000000 (transparent black)." Go re-slices
                // up to 256 4-byte pixels within the capacity it asked `make` for, which is zeroed;
                // resizing with zeros is the same bytes.
                pix.resize(4 * 256, 0);
                t.pix = pix;
            }
            _ => {}
        }
        Ok((t, w))
    }

    /// `decoder.decodeCodeLengths`: a Huffman tree's code lengths, themselves Huffman-encoded
    /// (section 5.2.2).
    fn decode_code_lengths(
        &mut self,
        dst: &mut [u32],
        code_length_code_lengths: &[u32],
    ) -> Result<(), Error> {
        let mut h = HTree::default();
        h.build(code_length_code_lengths)?;

        let mut max_symbol = dst.len();
        if self.read(1)? != 0 {
            let n = 2 + 2 * self.read(3)?;
            let ms = self.read(n)?;
            max_symbol = ms as usize + 2;
            if max_symbol > dst.len() {
                return Err(Error::InvalidCodeLengths);
            }
        }

        // The spec says that "if code 16 [meaning repeat] is used before a non-zero value has been
        // emitted, a value of 8 is repeated."
        let mut prev_code_length: u32 = 8;

        let mut symbol = 0usize;
        while symbol < dst.len() {
            if max_symbol == 0 {
                break;
            }
            max_symbol -= 1;
            let code_length = h.next(self)?;
            if code_length < REPEATS_CODE_LENGTH {
                dst[symbol] = code_length;
                symbol += 1;
                if code_length != 0 {
                    prev_code_length = code_length;
                }
                continue;
            }

            let idx = (code_length - REPEATS_CODE_LENGTH) as usize;
            // `h` was built over the 19-symbol code-length alphabet, so `code_length` is at most
            // 18 and `idx` at most 2.
            let Some(&bits) = REPEAT_BITS.get(idx) else {
                return Err(Error::InvalidCodeLengths);
            };
            let mut repeat = self.read(u32::from(bits))? + u32::from(REPEAT_OFFSETS[idx]);
            if symbol + repeat as usize > dst.len() {
                return Err(Error::InvalidCodeLengths);
            }
            // A code length of 16 repeats the previous non-zero code. 17 or 18 repeat zeroes.
            let cl = if code_length == 16 {
                prev_code_length
            } else {
                0
            };
            while repeat > 0 {
                repeat -= 1;
                dst[symbol] = cl;
                symbol += 1;
            }
        }
        Ok(())
    }

    /// `decoder.decodeHuffmanTree`.
    fn decode_huffman_tree(&mut self, h: &mut HTree, alphabet_size: u32) -> Result<(), Error> {
        if self.read(1)? != 0 {
            let n_symbols = self.read(1)? + 1;
            let first_symbol_length_code = 7 * self.read(1)? + 1;
            let mut symbols = [0u32; 2];
            symbols[0] = self.read(first_symbol_length_code)?;
            if n_symbols == 2 {
                symbols[1] = self.read(8)?;
            }
            return h.build_simple(n_symbols, symbols, alphabet_size);
        }

        let n_codes = self.read(4)? + 4;
        if n_codes as usize > CODE_LENGTH_CODE_ORDER.len() {
            return Err(Error::InvalidHuffmanTree);
        }
        let mut code_length_code_lengths = [0u32; CODE_LENGTH_CODE_ORDER.len()];
        for i in 0..n_codes as usize {
            code_length_code_lengths[CODE_LENGTH_CODE_ORDER[i] as usize] = self.read(3)?;
        }
        let mut code_lengths = vec![0u32; alphabet_size as usize];
        self.decode_code_lengths(&mut code_lengths, &code_length_code_lengths)?;
        h.build(&code_lengths)
    }

    /// `decoder.decodeHuffmanGroups`: the one or more `hGroup`s used to decode the pixel data. If
    /// one group covers the whole image, `h_pix` is empty and `h_bits` zero; otherwise `h_pix` is
    /// the meta-image mapping tiles to group indices and `h_bits` the log-2 tile size.
    #[allow(clippy::type_complexity)]
    fn decode_huffman_groups(
        &mut self,
        w: i32,
        h: i32,
        top_level: bool,
        cc_bits: u32,
    ) -> Result<(Vec<[HTree; N_HUFF]>, Vec<u8>, u32), Error> {
        let mut max_h_group_index = 0usize;
        let mut h_pix = Vec::new();
        let mut h_bits = 0u32;
        if top_level && self.read(1)? != 0 {
            h_bits = self.read(3)? + 2;
            h_pix = self.decode_pix(n_tiles(w, h_bits), n_tiles(h, h_bits), 0, false)?;
            let mut p = 0;
            while p < h_pix.len() {
                let i = usize::from(h_pix[p]) << 8 | usize::from(h_pix[p + 1]);
                if max_h_group_index < i {
                    max_h_group_index = i;
                }
                p += 4;
            }
        }
        let mut h_groups: Vec<[HTree; N_HUFF]> = Vec::with_capacity(max_h_group_index + 1);
        for _ in 0..max_h_group_index + 1 {
            let mut group: [HTree; N_HUFF] = std::array::from_fn(|_| HTree::default());
            for (j, &size) in ALPHABET_SIZES.iter().enumerate() {
                let mut alphabet_size = size;
                if j == 0 && cc_bits > 0 {
                    alphabet_size += 1 << cc_bits;
                }
                self.decode_huffman_tree(&mut group[j], alphabet_size)?;
            }
            h_groups.push(group);
        }
        Ok((h_groups, h_pix, h_bits))
    }

    /// `decoder.decodePix`: pixel data (section 5.2.2).
    fn decode_pix(
        &mut self,
        w: i32,
        h: i32,
        min_cap: i32,
        top_level: bool,
    ) -> Result<Vec<u8>, Error> {
        // Decode the color cache parameters.
        let (mut cc_bits, mut cc_shift) = (0u32, 0u32);
        let mut cc_entries: Vec<u32> = Vec::new();
        let use_color_cache = self.read(1)?;
        if use_color_cache != 0 {
            cc_bits = self.read(4)?;
            if !(1..=11).contains(&cc_bits) {
                return Err(Error::InvalidColorCacheParameters);
            }
            cc_shift = 32 - cc_bits;
            cc_entries = vec![0u32; 1usize << cc_bits];
        }

        // Decode the Huffman groups.
        let (h_groups, h_pix, h_bits) = self.decode_huffman_groups(w, h, top_level, cc_bits)?;
        let (mut h_mask, mut tiles_per_row) = (0i32, 0i32);
        if h_bits != 0 {
            h_mask = (1 << h_bits) - 1;
            tiles_per_row = n_tiles(w, h_bits);
        }

        // Decode the pixels. `w` and `h` are 14-bit fields plus one, so `4*w*h` is at most 2^30 and
        // the allocation below cannot overflow.
        let n = (4 * w * h) as usize;
        let mut pix: Vec<u8> = Vec::with_capacity(n.max(min_cap.max(0) as usize));
        pix.resize(n, 0);
        let (mut p, mut cached_p) = (0usize, 0usize);
        let (mut x, mut y) = (0i32, 0i32);
        let mut hg = 0usize;
        let mut lookup_hg = h_mask != 0;
        while p < pix.len() {
            if lookup_hg {
                let i = (4 * (tiles_per_row * (y >> h_bits) + (x >> h_bits))) as usize;
                hg = usize::from(h_pix[i]) << 8 | usize::from(h_pix[i + 1]);
            }
            let group = &h_groups[hg];

            let green = group[HUFF_GREEN].next(self)?;
            if green < N_LITERAL_CODES {
                // We have a literal pixel.
                let red = group[HUFF_RED].next(self)?;
                let blue = group[HUFF_BLUE].next(self)?;
                let alpha = group[HUFF_ALPHA].next(self)?;
                pix[p] = red as u8;
                pix[p + 1] = green as u8;
                pix[p + 2] = blue as u8;
                pix[p + 3] = alpha as u8;
                p += 4;

                x += 1;
                if x == w {
                    x = 0;
                    y += 1;
                }
                lookup_hg = h_mask != 0 && x & h_mask == 0;
            } else if green < N_LITERAL_CODES + N_LENGTH_CODES {
                // We have a LZ77 backwards reference.
                let length = self.lz77_param(green - N_LITERAL_CODES)?;
                let dist_sym = group[HUFF_DISTANCE].next(self)?;
                let dist_code = self.lz77_param(dist_sym)?;
                let dist = distance_map(w, dist_code);
                let p64 = p as i64;
                let p_end = p64 + 4 * i64::from(length);
                let q = p64 - 4 * i64::from(dist);
                let q_end = p_end - 4 * i64::from(dist);
                if p64 < 0 || (pix.len() as i64) < p_end || q < 0 || (pix.len() as i64) < q_end {
                    return Err(Error::InvalidLz77Parameters);
                }
                // Byte by byte and forwards, so an overlapping reference repeats what it just
                // wrote — that is how VP8L spells a run.
                let mut q = q as usize;
                let p_end = p_end as usize;
                while p < p_end {
                    let b = pix[q];
                    pix[p] = b;
                    p += 1;
                    q += 1;
                }

                x += length as i32;
                while x >= w {
                    x -= w;
                    y += 1;
                }
                lookup_hg = h_mask != 0;
            } else {
                // We have a color cache lookup. First, insert previous pixels into the cache. Note
                // that VP8L assumes ARGB order, but the Go image.RGBA type is in RGBA order.
                while cached_p < p {
                    let argb = u32::from(pix[cached_p]) << 16
                        | u32::from(pix[cached_p + 1]) << 8
                        | u32::from(pix[cached_p + 2])
                        | u32::from(pix[cached_p + 3]) << 24;
                    let i = (argb.wrapping_mul(COLOR_CACHE_MULTIPLIER) >> cc_shift) as usize;
                    cc_entries[i] = argb;
                    cached_p += 4;
                }
                let green = green - (N_LITERAL_CODES + N_LENGTH_CODES);
                if green as usize >= cc_entries.len() {
                    return Err(Error::InvalidColorCacheIndex);
                }
                let argb = cc_entries[green as usize];
                pix[p] = (argb >> 16) as u8;
                pix[p + 1] = (argb >> 8) as u8;
                pix[p + 2] = argb as u8;
                pix[p + 3] = (argb >> 24) as u8;
                p += 4;

                x += 1;
                if x == w {
                    x = 0;
                    y += 1;
                }
                lookup_hg = h_mask != 0 && x & h_mask == 0;
            }
        }
        Ok(pix)
    }

    /// `decoder.lz77Param`: the next LZ77 parameter, a length or a distance (section 4.2.2).
    fn lz77_param(&mut self, symbol: u32) -> Result<u32, Error> {
        if symbol < 4 {
            return Ok(symbol + 1);
        }
        let extra_bits = (symbol - 2) >> 1;
        let offset = (2 + (symbol & 1)) << extra_bits;
        let n = self.read(extra_bits)?;
        Ok(offset + n + 1)
    }
}

/// `repeatsCodeLength`: the minimum code length for repeated codes.
const REPEATS_CODE_LENGTH: u32 = 16;

/// The magic numbers at the end of section 5.2.2. The three-length arrays apply to code lengths at
/// or above [`REPEATS_CODE_LENGTH`].
const CODE_LENGTH_CODE_ORDER: [u8; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];
const REPEAT_BITS: [u8; 3] = [2, 3, 7];
const REPEAT_OFFSETS: [u8; 3] = [3, 3, 11];

const HUFF_GREEN: usize = 0;
const HUFF_RED: usize = 1;
const HUFF_BLUE: usize = 2;
const HUFF_ALPHA: usize = 3;
const HUFF_DISTANCE: usize = 4;
const N_HUFF: usize = 5;

const N_LITERAL_CODES: u32 = 256;
const N_LENGTH_CODES: u32 = 24;
const N_DISTANCE_CODES: u32 = 40;

const ALPHABET_SIZES: [u32; N_HUFF] = [
    N_LITERAL_CODES + N_LENGTH_CODES,
    N_LITERAL_CODES,
    N_LITERAL_CODES,
    N_LITERAL_CODES,
    N_DISTANCE_CODES,
];

/// `decodeHeader`: the VP8L header — a magic byte, two 14-bit dimensions minus one, an ignored
/// alpha hint and a 3-bit version.
fn decode_header(r: &mut dyn ByteRead) -> Result<(BitReader<'_>, i32, i32), Error> {
    let mut d = BitReader {
        r,
        bits: 0,
        n_bits: 0,
    };
    if d.read(8)? != 0x2f {
        return Err(Error::InvalidHeader);
    }
    let width = d.read(14)? + 1;
    let height = d.read(14)? + 1;
    d.read(1)?; // Read and ignore the hasAlpha hint.
    if d.read(3)? != 0 {
        return Err(Error::InvalidVersion);
    }
    Ok((d, width as i32, height as i32))
}

/// The dimensions of a VP8L image. Its colour model is always `color.NRGBAModel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i32,
    pub height: i32,
}

/// Port of `vp8l.DecodeConfig`.
pub fn decode_config(r: &mut dyn ByteRead) -> Result<Config, Error> {
    let (_, w, h) = decode_header(r)?;
    Ok(Config {
        width: w,
        height: h,
    })
}

/// Port of `vp8l.Decode`.
pub fn decode(r: &mut dyn ByteRead) -> Result<Image, Error> {
    let (mut d, mut w, h) = decode_header(r)?;
    // Decode the transforms.
    let mut transforms: Vec<Transform> = Vec::with_capacity(N_TRANSFORM_TYPES);
    let mut transforms_seen = [false; N_TRANSFORM_TYPES];
    let original_w = w;
    loop {
        if d.read(1)? == 0 {
            break;
        }
        let (t, new_w) = d.decode_transform(w, h)?;
        w = new_w;
        let seen = &mut transforms_seen[t.transform_type as usize];
        if *seen {
            return Err(Error::RepeatedTransform);
        }
        *seen = true;
        transforms.push(t);
    }
    // Decode the transformed pixels.
    let mut pix = d.decode_pix(w, h, 0, true)?;
    // Apply the inverse transformations.
    for t in transforms.iter().rev() {
        pix = inverse_transform(t, pix, h);
    }
    Ok(Image::Nrgba(Pixels {
        pix,
        stride: 4 * original_w as usize,
        rect: Rect::new(0, 0, i64::from(original_w), i64::from(h)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::{BufReader, BytesReader};

    fn decode_bytes(data: &[u8]) -> Result<Image, Error> {
        let mut br = BufReader::new(BytesReader::new(data));
        decode(&mut br)
    }

    fn config_bytes(data: &[u8]) -> Result<Config, Error> {
        let mut br = BufReader::new(BytesReader::new(data));
        decode_config(&mut br)
    }

    #[test]
    fn distance_map_matches_gos_table_walk() {
        // Past the table, the code is its own offset minus the table length.
        assert_eq!(distance_map(10, 121), 1);
        assert_eq!(distance_map(10, 200), 80);
        // Code 1 is table entry 0 = 0x18: y 1, x 8-8 = 0, so the distance is one row.
        assert_eq!(distance_map(10, 1), 10);
        // Code 2 is 0x07: y 0, x 8-7 = 1.
        assert_eq!(distance_map(10, 2), 1);
        // Code 97 is 0x00: y 0, x 8, giving 8 — but with a narrow image the formula can reach
        // zero or less, and Go floors it at 1.
        assert_eq!(distance_map(1, 97), 8);
        assert_eq!(distance_map(1, 1), 1);
        // 0x40 (code 111): y 4, x 8. A width of 1 gives 12; the minimum only bites on negatives.
        assert_eq!(distance_map(1, 111), 12);
    }

    #[test]
    fn lz77_param_matches_the_spec() {
        let mut br = BufReader::new(BytesReader::new(&[0xff; 8]));
        let mut d = BitReader {
            r: &mut br,
            bits: 0,
            n_bits: 0,
        };
        for s in 0..4u32 {
            assert_eq!(d.lz77_param(s).unwrap(), s + 1);
        }
        // symbol 4: extraBits 1, offset (2+0)<<1 = 4; the next bit is 1, so 4+1+1 = 6.
        assert_eq!(d.lz77_param(4).unwrap(), 6);
        // symbol 5: extraBits 1, offset (2+1)<<1 = 6; 6+1+1 = 8.
        assert_eq!(d.lz77_param(5).unwrap(), 8);
    }

    #[test]
    fn the_header_is_forty_bits() {
        assert_eq!(
            config_bytes(&[0x2f, 0x00, 0x00, 0x00, 0x00]).unwrap(),
            Config {
                width: 1,
                height: 1
            }
        );
        // widthMinusOne = 0x3fff, heightMinusOne = 0x3fff.
        assert_eq!(
            config_bytes(&[0x2f, 0xff, 0xff, 0xff, 0x0f]).unwrap(),
            Config {
                width: 16384,
                height: 16384
            }
        );
        assert_eq!(
            config_bytes(&[0x2e, 0x00, 0x00, 0x00, 0x00]).err(),
            Some(Error::InvalidHeader)
        );
        // Version 1 sits in the top three bits of the fifth byte.
        assert_eq!(
            config_bytes(&[0x2f, 0x00, 0x00, 0x00, 0x20]).err(),
            Some(Error::InvalidVersion)
        );
        for n in 0..5 {
            assert_eq!(
                config_bytes(&[0x2f, 0x00, 0x00, 0x00, 0x00][..n]).err(),
                Some(Error::Io(goread::Error::UnexpectedEof)),
                "{n}"
            );
        }
    }

    #[test]
    fn the_colour_cache_parameters_are_checked() {
        // Byte five: bit 0 clears the transform loop, bit 1 sets useColorCache, bits 2..5 are
        // ccBits.
        assert_eq!(
            decode_bytes(&[0x2f, 0, 0, 0, 0, 0x02]).err(),
            Some(Error::InvalidColorCacheParameters)
        );
        assert_eq!(
            decode_bytes(&[0x2f, 0, 0, 0, 0, 0x02 | 12 << 2]).err(),
            Some(Error::InvalidColorCacheParameters)
        );
        // ccBits 1 and 11 are in range, so the stream gets as far as the Huffman groups.
        for cc in [1u8, 11] {
            assert_eq!(
                decode_bytes(&[0x2f, 0, 0, 0, 0, 0x02 | cc << 2]).err(),
                Some(Error::Io(goread::Error::UnexpectedEof)),
                "{cc}"
            );
        }
    }

    #[test]
    fn a_transform_cannot_repeat() {
        // Two subtract-green transforms back to back.
        assert_eq!(
            decode_bytes(&[0x2f, 0, 0, 0, 0, 0x2d]).err(),
            Some(Error::RepeatedTransform)
        );
        // One is fine, and the stream then runs out in decodePix.
        assert_eq!(
            decode_bytes(&[0x2f, 0, 0, 0, 0, 0x05]).err(),
            Some(Error::Io(goread::Error::UnexpectedEof))
        );
    }
}
