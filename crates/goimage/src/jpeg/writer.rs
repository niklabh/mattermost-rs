//! Port of Go's `image/jpeg/writer.go` (go1.26.4): a baseline, 4:2:0 (or grayscale) JPEG encoder
//! with the fixed Annex K Huffman tables and quality-scaled Annex K quantization tables.
//!
//! Every byte is decided here: the colour conversion path Go takes by the image's dynamic type,
//! the edge replication of partial blocks, the 2×2 chroma averaging, the rounding division used
//! for quantization, the 0xff byte stuffing and the final 1-bit padding.
//!
//! Go wraps the destination in a `bufio.Writer` (a `bytes.Buffer` has no `Flush`). That only
//! changes where the destination's `Write` calls fall, never the bytes, and nothing downstream of
//! a JPEG encode observes call boundaries, so this encoder assembles the stream in memory and
//! hands it to the sink in one write.

use crate::image::{Image, Pixels, YCbCr, rgb_to_ycbcr};
use crate::sink::Sink;

use super::fdct::{BLOCK_SIZE, Block, fdct};

/// Port of `DefaultQuality` (writer.go:582).
pub const DEFAULT_QUALITY: i32 = 75;

/// Port of `jpeg.Options` (writer.go:586).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// 1..=100; out-of-range values are clamped.
    pub quality: i32,
}

/// The ways `jpeg.Encode` can fail.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JpegEncodeError {
    /// writer.go:593.
    #[error("jpeg: image is too large to encode")]
    TooLarge,
    /// A `Paletted` image with an empty palette: Go's `At` returns a nil `color.Color` and the
    /// generic path panics calling `RGBA()` on it. Not a Go error value — a refusal to go on.
    #[error("jpeg: paletted image has an empty palette")]
    EmptyPalette,
}

// Markers (reader.go:48-59).
const SOF0_MARKER: u8 = 0xc0;
const DHT_MARKER: u8 = 0xc4;
const DQT_MARKER: u8 = 0xdb;

/// Port of `unzig` (reader.go:78): zig-zag index → natural index.
const UNZIG: [usize; BLOCK_SIZE] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Port of `div` (writer.go:16): a/b rounded to nearest, halves away from zero.
fn div(a: i32, b: i32) -> i32 {
    if a >= 0 {
        a.wrapping_add(b >> 1) / b
    } else {
        -((a.wrapping_neg().wrapping_add(b >> 1)) / b)
    }
}

/// Port of `bitCount` (writer.go:24): bits needed to hold a byte.
fn bit_count(a: usize) -> u32 {
    usize::BITS - a.leading_zeros()
}

/// Port of `unscaledQuant` (writer.go:54), zig-zag order.
const UNSCALED_QUANT: [[u8; BLOCK_SIZE]; 2] = [
    [
        16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35,
        37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80,
        109, 81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99,
    ],
    [
        17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ],
];

/// Port of `huffmanSpec` (writer.go:94).
struct HuffmanSpec {
    count: [u8; 16],
    value: &'static [u8],
}

/// Port of `theHuffmanSpec` (writer.go:111): luminance DC, luminance AC, chrominance DC,
/// chrominance AC — section K.3 of the spec.
const THE_HUFFMAN_SPEC: [HuffmanSpec; 4] = [
    HuffmanSpec {
        count: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        value: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    },
    HuffmanSpec {
        count: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125],
        value: &[
            0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51,
            0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1,
            0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
            0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57,
            0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92,
            0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
            0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8,
            0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    },
    HuffmanSpec {
        count: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
        value: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    },
    HuffmanSpec {
        count: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119],
        value: &[
            0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
            0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09,
            0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25,
            0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38,
            0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
            0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
            0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
            0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    },
];

/// Port of `huffmanLUT.init` (writer.go:189): value → (bit length << 24 | codeword).
fn huffman_lut(s: &HuffmanSpec) -> Vec<u32> {
    let max_value = s.value.iter().copied().max().map_or(0, usize::from);
    let mut lut = vec![0u32; max_value + 1];
    let (mut code, mut k) = (0u32, 0usize);
    for (i, &n) in s.count.iter().enumerate() {
        let n_bits = ((i + 1) as u32) << 24;
        for _ in 0..n {
            if let Some(&v) = s.value.get(k) {
                lut[usize::from(v)] = n_bits | code;
            }
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    lut
}

/// Port of the `encoder` struct (writer.go:222): the bit accumulator, the scaled tables and the
/// output being assembled.
struct Encoder {
    out: Vec<u8>,
    bits: u32,
    n_bits: u32,
    quant: [[u8; BLOCK_SIZE]; 2],
    lut: [Vec<u32>; 4],
}

impl Encoder {
    /// Port of `emit` (writer.go:260). Precondition: `bits < 1<<n_bits && n_bits <= 16`.
    fn emit(&mut self, bits: u32, n_bits: u32) {
        let mut n_bits = n_bits + self.n_bits;
        let mut bits = (bits << (32 - n_bits)) | self.bits;
        while n_bits >= 8 {
            let b = (bits >> 24) as u8;
            self.out.push(b);
            if b == 0xff {
                self.out.push(0x00);
            }
            bits <<= 8;
            n_bits -= 8;
        }
        self.bits = bits;
        self.n_bits = n_bits;
    }

    /// Port of `emitHuff` (writer.go:277).
    fn emit_huff(&mut self, h: usize, value: i32) {
        let x = self.lut[h].get(value as usize).copied().unwrap_or(0);
        self.emit(x & ((1 << 24) - 1), x >> 24);
    }

    /// Port of `emitHuffRLE` (writer.go:284).
    fn emit_huff_rle(&mut self, h: usize, run_length: i32, value: i32) {
        let (a, b) = if value < 0 {
            (value.wrapping_neg(), value.wrapping_sub(1))
        } else {
            (value, value)
        };
        let n_bits = if a < 0x100 {
            bit_count(a as usize)
        } else {
            8 + bit_count((a >> 8) as usize)
        };
        self.emit_huff(h, (run_length << 4) | n_bits as i32);
        if n_bits > 0 {
            self.emit((b as u32) & ((1 << n_bits) - 1), n_bits);
        }
    }

    /// Port of `writeMarkerHeader` (writer.go:303).
    fn write_marker_header(&mut self, marker: u8, markerlen: usize) {
        self.out
            .extend_from_slice(&[0xff, marker, (markerlen >> 8) as u8, markerlen as u8]);
    }

    /// Port of `writeDQT` (writer.go:312).
    fn write_dqt(&mut self) {
        self.write_marker_header(DQT_MARKER, 2 + 2 * (1 + BLOCK_SIZE));
        for i in 0..2 {
            self.out.push(i as u8);
            let q = self.quant[i];
            self.out.extend_from_slice(&q);
        }
    }

    /// Port of `writeSOF0` (writer.go:322).
    fn write_sof0(&mut self, w: i64, h: i64, n_component: usize) {
        self.write_marker_header(SOF0_MARKER, 8 + 3 * n_component);
        self.out
            .extend_from_slice(&[8, (h >> 8) as u8, h as u8, (w >> 8) as u8, w as u8]);
        self.out.push(n_component as u8);
        if n_component == 1 {
            self.out.extend_from_slice(&[1, 0x11, 0x00]);
        } else {
            for i in 0..n_component {
                self.out
                    .extend_from_slice(&[(i + 1) as u8, [0x22, 0x11, 0x11][i], [0, 1, 1][i]]);
            }
        }
    }

    /// Port of `writeDHT` (writer.go:348).
    fn write_dht(&mut self, n_component: usize) {
        let specs = if n_component == 1 {
            &THE_HUFFMAN_SPEC[..2]
        } else {
            &THE_HUFFMAN_SPEC[..]
        };
        let markerlen = 2 + specs.iter().map(|s| 1 + 16 + s.value.len()).sum::<usize>();
        self.write_marker_header(DHT_MARKER, markerlen);
        for (i, s) in specs.iter().enumerate() {
            self.out.push([0x00, 0x10, 0x01, 0x11][i]);
            self.out.extend_from_slice(&s.count);
            self.out.extend_from_slice(s.value);
        }
    }

    /// Port of `writeBlock` (writer.go:371): FDCT, quantize, emit; returns the quantized DC.
    fn write_block(&mut self, b: &mut Block, q: usize, prev_dc: i32) -> i32 {
        fdct(b);
        let dc = div(b[0], 8 * i32::from(self.quant[q][0]));
        self.emit_huff_rle(2 * q, 0, dc.wrapping_sub(prev_dc));
        let h = 2 * q + 1;
        let mut run_length = 0;
        for zig in 1..BLOCK_SIZE {
            let ac = div(b[UNZIG[zig]], 8 * i32::from(self.quant[q][zig]));
            if ac == 0 {
                run_length += 1;
            } else {
                while run_length > 15 {
                    self.emit_huff(h, 0xf0);
                    run_length -= 16;
                }
                self.emit_huff_rle(h, run_length, ac);
                run_length = 0;
            }
        }
        if run_length > 0 {
            self.emit_huff(h, 0x00);
        }
        dc
    }
}

/// Port of `toYCbCr` (writer.go:398): the generic path, through `At(x, y).RGBA()`.
fn to_ycbcr(
    m: &Image,
    px: i64,
    py: i64,
    yb: &mut Block,
    cbb: &mut Block,
    crb: &mut Block,
) -> Result<(), JpegEncodeError> {
    let b = m.bounds();
    let (xmax, ymax) = (b.max_x - 1, b.max_y - 1);
    for j in 0..8 {
        for i in 0..8 {
            let c = m
                .at((px + i).min(xmax), (py + j).min(ymax))
                .ok_or(JpegEncodeError::EmptyPalette)?;
            let (r, g, bb, _) = c.rgba();
            let (yy, cb, cr) = rgb_to_ycbcr((r >> 8) as u8, (g >> 8) as u8, (bb >> 8) as u8);
            let k = (8 * j + i) as usize;
            yb[k] = i32::from(yy);
            cbb[k] = i32::from(cb);
            crb[k] = i32::from(cr);
        }
    }
    Ok(())
}

/// Port of `grayToY` (writer.go:414).
fn gray_to_y(m: &Pixels, px: i64, py: i64, yb: &mut Block) {
    let (xmax, ymax) = (m.rect.max_x - 1, m.rect.max_y - 1);
    for j in 0..8 {
        for i in 0..8 {
            yb[(8 * j + i) as usize] =
                i32::from(m.pix[m.offset((px + i).min(xmax), (py + j).min(ymax), 1)]);
        }
    }
}

/// Port of `rgbaToYCbCr` (writer.go:428): reads the premultiplied bytes directly.
fn rgba_to_ycbcr(m: &Pixels, px: i64, py: i64, yb: &mut Block, cbb: &mut Block, crb: &mut Block) {
    let (xmax, ymax) = (m.rect.max_x - 1, m.rect.max_y - 1);
    for j in 0..8 {
        let sj = (py + j).min(ymax);
        for i in 0..8 {
            let sx = (px + i).min(xmax);
            let o = m.offset(sx, sj, 4);
            let (yy, cb, cr) = rgb_to_ycbcr(m.pix[o], m.pix[o + 1], m.pix[o + 2]);
            let k = (8 * j + i) as usize;
            yb[k] = i32::from(yy);
            cbb[k] = i32::from(cb);
            crb[k] = i32::from(cr);
        }
    }
}

/// Port of `yCbCrToYCbCr` (writer.go:452).
fn ycbcr_to_ycbcr(m: &YCbCr, px: i64, py: i64, yb: &mut Block, cbb: &mut Block, crb: &mut Block) {
    let (xmax, ymax) = (m.rect.max_x - 1, m.rect.max_y - 1);
    for j in 0..8 {
        let sy = (py + j).min(ymax);
        for i in 0..8 {
            let sx = (px + i).min(xmax);
            let yi = m.y_offset(sx, sy);
            let ci = m.c_offset(sx, sy);
            let k = (8 * j + i) as usize;
            yb[k] = i32::from(m.y[yi]);
            cbb[k] = i32::from(m.cb[ci]);
            crb[k] = i32::from(m.cr[ci]);
        }
    }
}

/// Port of `scale` (writer.go:475): four 8×8 blocks (a 16×16 region) averaged down to one.
fn scale(dst: &mut Block, src: &[Block; 4]) {
    for (i, s) in src.iter().enumerate() {
        let dst_off = ((i & 2) << 4) | ((i & 1) << 2);
        for y in 0..4 {
            for x in 0..4 {
                let j = 16 * y + 2 * x;
                let sum = s[j] + s[j + 1] + s[j + 8] + s[j + 9];
                dst[8 * y + x + dst_off] = (sum + 2) >> 2;
            }
        }
    }
}

/// Port of `sosHeaderY` (writer.go:497).
const SOS_HEADER_Y: [u8; 10] = [0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00];

/// Port of `sosHeaderYCbCr` (writer.go:510).
const SOS_HEADER_YCBCR: [u8; 14] = [
    0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
];

impl Encoder {
    /// Port of `writeSOS` (writer.go:516).
    fn write_sos(&mut self, m: &Image) -> Result<(), JpegEncodeError> {
        let bounds = m.bounds();
        let mut b: Block = [0; BLOCK_SIZE];
        if let Image::Gray(g) = m {
            self.out.extend_from_slice(&SOS_HEADER_Y);
            let mut prev_dc_y = 0;
            let mut y = bounds.min_y;
            while y < bounds.max_y {
                let mut x = bounds.min_x;
                while x < bounds.max_x {
                    gray_to_y(g, x, y, &mut b);
                    prev_dc_y = self.write_block(&mut b, 0, prev_dc_y);
                    x += 8;
                }
                y += 8;
            }
        } else {
            self.out.extend_from_slice(&SOS_HEADER_YCBCR);
            let mut cb: [Block; 4] = [[0; BLOCK_SIZE]; 4];
            let mut cr: [Block; 4] = [[0; BLOCK_SIZE]; 4];
            let (mut prev_dc_y, mut prev_dc_cb, mut prev_dc_cr) = (0, 0, 0);
            let mut y = bounds.min_y;
            while y < bounds.max_y {
                let mut x = bounds.min_x;
                while x < bounds.max_x {
                    for i in 0..4 {
                        let x_off = ((i & 1) * 8) as i64;
                        let y_off = ((i & 2) * 4) as i64;
                        let (px, py) = (x + x_off, y + y_off);
                        match m {
                            Image::Rgba(p) => {
                                rgba_to_ycbcr(p, px, py, &mut b, &mut cb[i], &mut cr[i])
                            }
                            Image::YCbCr(p) => {
                                ycbcr_to_ycbcr(p, px, py, &mut b, &mut cb[i], &mut cr[i])
                            }
                            _ => to_ycbcr(m, px, py, &mut b, &mut cb[i], &mut cr[i])?,
                        }
                        prev_dc_y = self.write_block(&mut b, 0, prev_dc_y);
                    }
                    scale(&mut b, &cb);
                    prev_dc_cb = self.write_block(&mut b, 1, prev_dc_cb);
                    scale(&mut b, &cr);
                    prev_dc_cr = self.write_block(&mut b, 1, prev_dc_cr);
                    x += 16;
                }
                y += 16;
            }
        }
        // Pad the last byte with 1's.
        self.emit(0x7f, 7);
        Ok(())
    }
}

/// Port of `jpeg.Encode` (writer.go:591): `m` as a baseline JPEG, 4:2:0 unless `m` is a
/// `Gray`. `None` options mean [`DEFAULT_QUALITY`]; a given quality is clamped to 1..=100.
///
/// Nothing is written to `w` on error. (Go's only error, the size check, also comes before
/// any write.)
pub fn encode(
    w: &mut impl Sink,
    m: &Image,
    options: Option<Options>,
) -> Result<(), JpegEncodeError> {
    let b = m.bounds();
    if b.dx() >= 1 << 16 || b.dy() >= 1 << 16 {
        return Err(JpegEncodeError::TooLarge);
    }
    let quality = options.map_or(DEFAULT_QUALITY, |o| o.quality.clamp(1, 100));
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    let mut quant = [[0u8; BLOCK_SIZE]; 2];
    for (i, table) in quant.iter_mut().enumerate() {
        for (j, q) in table.iter_mut().enumerate() {
            let x = (i32::from(UNSCALED_QUANT[i][j]) * scale + 50) / 100;
            *q = x.clamp(1, 255) as u8;
        }
    }
    let n_component = if matches!(m, Image::Gray(_)) { 1 } else { 3 };
    let mut e = Encoder {
        out: Vec::new(),
        bits: 0,
        n_bits: 0,
        quant,
        lut: [
            huffman_lut(&THE_HUFFMAN_SPEC[0]),
            huffman_lut(&THE_HUFFMAN_SPEC[1]),
            huffman_lut(&THE_HUFFMAN_SPEC[2]),
            huffman_lut(&THE_HUFFMAN_SPEC[3]),
        ],
    };
    e.out.extend_from_slice(&[0xff, 0xd8]);
    e.write_dqt();
    e.write_sof0(b.dx(), b.dy(), n_component);
    e.write_dht(n_component);
    e.write_sos(m)?;
    e.out.extend_from_slice(&[0xff, 0xd9]);
    w.write(&e.out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Pixels, Rect};

    #[test]
    fn div_rounds_halves_away_from_zero() {
        assert_eq!(div(5, 2), 3);
        assert_eq!(div(-5, 2), -3);
        assert_eq!(div(4, 8), 1);
        assert_eq!(div(-4, 8), -1);
        assert_eq!(div(3, 8), 0);
        assert_eq!(div(-3, 8), 0);
    }

    #[test]
    fn bit_count_matches_gos_table() {
        assert_eq!(bit_count(0), 0);
        assert_eq!(bit_count(1), 1);
        assert_eq!(bit_count(2), 2);
        assert_eq!(bit_count(3), 2);
        assert_eq!(bit_count(128), 8);
        assert_eq!(bit_count(255), 8);
    }

    fn gray(w: i64, h: i64) -> Image {
        Image::Gray(Pixels::new(Rect::new(0, 0, w, h), 1))
    }

    fn enc(m: &Image, q: Option<i32>) -> Result<Vec<u8>, JpegEncodeError> {
        let mut out = Vec::new();
        encode(&mut out, m, q.map(|quality| Options { quality }))?;
        Ok(out)
    }

    #[test]
    fn quality_is_clamped_to_1_and_100() {
        let m = gray(9, 9);
        assert_eq!(enc(&m, Some(-5)), enc(&m, Some(1)));
        assert_eq!(enc(&m, Some(0)), enc(&m, Some(1)));
        assert_eq!(enc(&m, Some(101)), enc(&m, Some(100)));
        assert_ne!(enc(&m, Some(1)), enc(&m, Some(2)));
        assert_ne!(enc(&m, Some(99)), enc(&m, Some(100)));
        assert_eq!(enc(&m, None), enc(&m, Some(DEFAULT_QUALITY)));
    }

    #[test]
    fn too_large_is_refused_before_any_write() {
        for (w, h) in [(1 << 16, 1), (1, 1 << 16)] {
            let m = Image::Gray(Pixels {
                pix: Vec::new(),
                stride: 0,
                rect: Rect::new(0, 0, w, h),
            });
            let mut out = Vec::new();
            assert_eq!(encode(&mut out, &m, None), Err(JpegEncodeError::TooLarge));
            assert!(out.is_empty());
        }
    }

    #[test]
    fn gray_is_one_component_and_everything_else_three() {
        let g = enc(&gray(1, 1), Some(90)).unwrap();
        // SOF0 component count sits 9 bytes after the SOF0 marker.
        let sof = g.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        assert_eq!(g[sof + 9], 1);
        let g16 = Image::Gray16(Pixels::new(Rect::new(0, 0, 1, 1), 2));
        let c = enc(&g16, Some(90)).unwrap();
        let sof = c.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        assert_eq!(c[sof + 9], 3);
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{assert_encoded, build, fixture};

    /// Every `jpeg.Encode` case in the oracle: generated input, quality, output bytes or error.
    #[test]
    fn every_oracle_encode_case_matches_go() {
        let cases = fixture("jpeg")["encode"].as_array().unwrap();
        assert!(cases.len() > 900, "{}", cases.len());
        for c in cases {
            let m = build(&c["spec"]);
            let q = c["quality"].as_i64().unwrap() as i32;
            let mut out = Vec::new();
            let res = encode(&mut out, &m, Some(Options { quality: q }));
            let name = format!("{} q{q}", c["spec"]);
            match c["err"].as_str() {
                Some(e) => assert_eq!(res.map_err(|e| e.to_string()), Err(e.to_owned()), "{name}"),
                None => {
                    res.unwrap();
                    assert_encoded(&name, &out, &c["output"]);
                }
            }
        }
    }
}
