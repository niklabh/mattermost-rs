//! Port of Go's `image/jpeg/reader.go` (go1.26.4): the marker loop, the byte buffer with its
//! byte-stuffing unread machinery, SOF/DQT/DRI/APP0/APP14 processing, the CMYK / YCbCrK / RGB
//! post-conversions, and `Decode` / `DecodeConfig`.
//!
//! The input is a whole byte slice. Go reads through an `io.Reader` into a 4096-byte buffer that
//! keeps its last two bytes on refill (so a byte-stuffed `0xff 0x00` can be unread across a
//! refill); the port keeps that buffer and its refill rule byte for byte, because the unread
//! arithmetic is defined against it. Where the underlying reader's `Read` returns fewer bytes than
//! asked (as the `bufio.Reader` that `image.Decode` wraps around its input does), nothing
//! observable changes: every read path loops until satisfied, and end of input is always
//! `io.ErrUnexpectedEOF`.

use super::huffman::Huffman;
use super::idct::Block;
use crate::image::{Image, Pixels, Rect, YCbCr, ycbcr_to_rgb};

/// A decoding failure, with Go's exact error text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JpegError {
    /// `jpeg.FormatError` (reader.go:19).
    #[error("invalid JPEG format: {0}")]
    Format(&'static str),
    /// `jpeg.UnsupportedError` (reader.go:24).
    #[error("unsupported JPEG feature: {0}")]
    Unsupported(&'static str),
    /// `io.ErrUnexpectedEOF`: the input ended inside a segment.
    #[error("unexpected EOF")]
    UnexpectedEof,
}

/// The colour model `DecodeConfig` reports (reader.go:790).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigModel {
    /// `color.GrayModel`.
    Gray,
    /// `color.YCbCrModel`.
    YCbCr,
    /// `color.RGBAModel` — a 3-component image `isRGB` says is RGB.
    Rgba,
    /// `color.CMYKModel`.
    Cmyk,
}

impl ConfigModel {
    /// The name the oracle records for the model.
    pub fn name(&self) -> &'static str {
        match self {
            ConfigModel::Gray => "gray",
            ConfigModel::YCbCr => "ycbcr",
            ConfigModel::Rgba => "rgba",
            ConfigModel::Cmyk => "cmyk",
        }
    }
}

/// `image.Config` as the JPEG decoder fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: ConfigModel,
}

pub(crate) const ERR_UNSUPPORTED_SUBSAMPLING: JpegError =
    JpegError::Unsupported("luma/chroma subsampling ratio");

/// Component specification, section B.2.2 (reader.go:30).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Component {
    pub(crate) h: usize,
    pub(crate) v: usize,
    pub(crate) c: u8,
    pub(crate) tq: u8,
}

pub(crate) const DC_TABLE: usize = 0;
pub(crate) const AC_TABLE: usize = 1;
const MAX_TC: u8 = 1;
pub(crate) const MAX_TH: u8 = 3;
const MAX_TQ: u8 = 3;
const MAX_COMPONENTS: usize = 4;

const SOF0_MARKER: u8 = 0xc0;
const SOF1_MARKER: u8 = 0xc1;
const SOF2_MARKER: u8 = 0xc2;
const DHT_MARKER: u8 = 0xc4;
pub(crate) const RST0_MARKER: u8 = 0xd0;
pub(crate) const RST7_MARKER: u8 = 0xd7;
const SOI_MARKER: u8 = 0xd8;
const EOI_MARKER: u8 = 0xd9;
const SOS_MARKER: u8 = 0xda;
const DQT_MARKER: u8 = 0xdb;
const DRI_MARKER: u8 = 0xdd;
const COM_MARKER: u8 = 0xfe;
const APP0_MARKER: u8 = 0xe0;
const APP14_MARKER: u8 = 0xee;
const APP15_MARKER: u8 = 0xef;

const ADOBE_TRANSFORM_UNKNOWN: u8 = 0;

pub(crate) const BLOCK_SIZE: usize = 64;

/// Port of `unzig` (reader.go:78): zig-zag index to natural index.
pub(crate) const UNZIG: [usize; BLOCK_SIZE] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Port of `bits` (reader.go:100): unprocessed bits taken from the byte stream.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Bits {
    /// Accumulator.
    pub(crate) a: u32,
    /// Mask: `1<<(n-1)` when n>0, 0 when n==0.
    pub(crate) m: u32,
    /// Number of unread bits in `a`.
    pub(crate) n: i32,
}

/// Port of the anonymous `bytes` struct inside `decoder` (reader.go:111).
pub(crate) struct ByteBuf {
    pub(crate) buf: [u8; 4096],
    pub(crate) i: usize,
    pub(crate) j: usize,
    pub(crate) n_unreadable: usize,
}

/// Port of `decoder` (reader.go:106).
pub(crate) struct Decoder<'a> {
    /// The unread remainder of the input — Go's `d.r`.
    src: &'a [u8],
    pub(crate) bits: Bits,
    pub(crate) bytes: ByteBuf,
    pub(crate) width: usize,
    pub(crate) height: usize,

    pub(crate) img1: Option<Pixels>,
    pub(crate) img3: Option<YCbCr>,
    pub(crate) black_pix: Option<Vec<u8>>,
    pub(crate) black_stride: usize,

    pub(crate) ri: usize,
    pub(crate) n_comp: usize,

    pub(crate) baseline: bool,
    pub(crate) progressive: bool,

    jfif: bool,
    adobe_transform_valid: bool,
    adobe_transform: u8,
    pub(crate) eob_run: u16,

    pub(crate) comp: [Component; MAX_COMPONENTS],
    pub(crate) prog_coeffs: [Option<Vec<Block>>; MAX_COMPONENTS],
    pub(crate) huff: [[Huffman; MAX_TH as usize + 1]; MAX_TC as usize + 1],
    pub(crate) quant: [Block; MAX_TQ as usize + 1],
    pub(crate) tmp: [u8; 2 * BLOCK_SIZE],
}

impl<'a> Decoder<'a> {
    fn new(src: &'a [u8]) -> Box<Decoder<'a>> {
        Box::new(Decoder {
            src,
            bits: Bits::default(),
            bytes: ByteBuf {
                buf: [0; 4096],
                i: 0,
                j: 0,
                n_unreadable: 0,
            },
            width: 0,
            height: 0,
            img1: None,
            img3: None,
            black_pix: None,
            black_stride: 0,
            ri: 0,
            n_comp: 0,
            baseline: false,
            progressive: false,
            jfif: false,
            adobe_transform_valid: false,
            adobe_transform: 0,
            eob_run: 0,
            comp: [Component::default(); MAX_COMPONENTS],
            prog_coeffs: [None, None, None, None],
            huff: Default::default(),
            quant: [[0; BLOCK_SIZE]; MAX_TQ as usize + 1],
            tmp: [0; 2 * BLOCK_SIZE],
        })
    }

    /// Port of `fill` (reader.go:146). Only called when no unread bytes remain.
    pub(crate) fn fill(&mut self) -> Result<(), JpegError> {
        // Go panics if i != j here; every caller guarantees it, and the port does too.
        if self.bytes.j > 2 {
            self.bytes.buf[0] = self.bytes.buf[self.bytes.j - 2];
            self.bytes.buf[1] = self.bytes.buf[self.bytes.j - 1];
            self.bytes.i = 2;
            self.bytes.j = 2;
        }
        let space = &mut self.bytes.buf[self.bytes.j..];
        let n = space.len().min(self.src.len());
        space[..n].copy_from_slice(&self.src[..n]);
        self.src = &self.src[n..];
        self.bytes.j += n;
        if n > 0 {
            return Ok(());
        }
        Err(JpegError::UnexpectedEof)
    }

    /// Port of `unreadByteStuffedByte` (reader.go:172).
    pub(crate) fn unread_byte_stuffed_byte(&mut self) {
        self.bytes.i -= self.bytes.n_unreadable;
        self.bytes.n_unreadable = 0;
        if self.bits.n >= 8 {
            self.bits.a >>= 8;
            self.bits.n -= 8;
            self.bits.m >>= 8;
        }
    }

    /// Port of `readByte` (reader.go:184).
    pub(crate) fn read_byte(&mut self) -> Result<u8, JpegError> {
        while self.bytes.i == self.bytes.j {
            self.fill()?;
        }
        let x = self.bytes.buf[self.bytes.i];
        self.bytes.i += 1;
        self.bytes.n_unreadable = 0;
        Ok(x)
    }

    /// Port of `readByteStuffedByte` (reader.go:201).
    pub(crate) fn read_byte_stuffed_byte(&mut self) -> Result<u8, JpegError> {
        if self.bytes.i + 2 <= self.bytes.j {
            let x = self.bytes.buf[self.bytes.i];
            self.bytes.i += 1;
            self.bytes.n_unreadable = 1;
            if x != 0xff {
                return Ok(x);
            }
            if self.bytes.buf[self.bytes.i] != 0x00 {
                return Err(ERR_MISSING_FF00);
            }
            self.bytes.i += 1;
            self.bytes.n_unreadable = 2;
            return Ok(0xff);
        }

        self.bytes.n_unreadable = 0;

        let x = self.read_byte()?;
        self.bytes.n_unreadable = 1;
        if x != 0xff {
            return Ok(x);
        }

        let x = self.read_byte()?;
        self.bytes.n_unreadable = 2;
        if x != 0x00 {
            return Err(ERR_MISSING_FF00);
        }
        Ok(0xff)
    }

    /// The "unread the overshot bytes" prologue shared by `readFull` and `ignore`.
    fn unread_overshoot(&mut self) {
        if self.bytes.n_unreadable != 0 {
            if self.bits.n >= 8 {
                self.unread_byte_stuffed_byte();
            }
            self.bytes.n_unreadable = 0;
        }
    }

    /// Port of `readFull` (reader.go:239) into `self.tmp[lo..hi]`.
    pub(crate) fn read_full_tmp(&mut self, lo: usize, hi: usize) -> Result<(), JpegError> {
        self.unread_overshoot();
        let mut p = lo;
        loop {
            let avail = self.bytes.j - self.bytes.i;
            let n = avail.min(hi - p);
            self.tmp[p..p + n].copy_from_slice(&self.bytes.buf[self.bytes.i..self.bytes.i + n]);
            p += n;
            self.bytes.i += n;
            if p == hi {
                break;
            }
            self.fill()?;
        }
        Ok(())
    }

    /// `readFull` into an arbitrary buffer (DHT's `h.vals`).
    pub(crate) fn read_full_into(&mut self, out: &mut [u8]) -> Result<(), JpegError> {
        self.unread_overshoot();
        let mut p = 0;
        loop {
            let avail = self.bytes.j - self.bytes.i;
            let n = avail.min(out.len() - p);
            out[p..p + n].copy_from_slice(&self.bytes.buf[self.bytes.i..self.bytes.i + n]);
            p += n;
            self.bytes.i += n;
            if p == out.len() {
                break;
            }
            self.fill()?;
        }
        Ok(())
    }

    /// Port of `ignore` (reader.go:263).
    pub(crate) fn ignore(&mut self, mut n: usize) -> Result<(), JpegError> {
        self.unread_overshoot();
        loop {
            let m = (self.bytes.j - self.bytes.i).min(n);
            self.bytes.i += m;
            n -= m;
            if n == 0 {
                break;
            }
            self.fill()?;
        }
        Ok(())
    }

    /// Port of `processSOF` (reader.go:287).
    fn process_sof(&mut self, n: usize) -> Result<(), JpegError> {
        if self.n_comp != 0 {
            return Err(JpegError::Format("multiple SOF markers"));
        }
        self.n_comp = match n {
            9 => 1,
            15 => 3,
            18 => 4,
            _ => return Err(JpegError::Unsupported("number of components")),
        };
        self.read_full_tmp(0, n)?;
        if self.tmp[0] != 8 {
            return Err(JpegError::Unsupported("precision"));
        }
        self.height = usize::from(self.tmp[1]) << 8 | usize::from(self.tmp[2]);
        self.width = usize::from(self.tmp[3]) << 8 | usize::from(self.tmp[4]);
        if usize::from(self.tmp[5]) != self.n_comp {
            return Err(JpegError::Format("SOF has wrong length"));
        }

        for i in 0..self.n_comp {
            self.comp[i].c = self.tmp[6 + 3 * i];
            for j in 0..i {
                if self.comp[i].c == self.comp[j].c {
                    return Err(JpegError::Format("repeated component identifier"));
                }
            }

            self.comp[i].tq = self.tmp[8 + 3 * i];
            if self.comp[i].tq > MAX_TQ {
                return Err(JpegError::Format("bad Tq value"));
            }

            let hv = self.tmp[7 + 3 * i];
            let (mut h, mut v) = (usize::from(hv >> 4), usize::from(hv & 0x0f));
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return Err(JpegError::Format("luma/chroma subsampling ratio"));
            }
            if h == 3 || v == 3 {
                return Err(ERR_UNSUPPORTED_SUBSAMPLING);
            }
            match self.n_comp {
                1 => {
                    // A single component is non-interleaved by definition; its (h, v) is
                    // effectively (1, 1) (reader.go:333).
                    h = 1;
                    v = 1;
                }
                3 => match i {
                    0 => {
                        if v == 4 {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                    1 => {
                        if self.comp[0].h % h != 0 || self.comp[0].v % v != 0 {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                    _ => {
                        if self.comp[1].h != h || self.comp[1].v != v {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                },
                _ => match i {
                    0 => {
                        if hv != 0x11 && hv != 0x22 {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                    1 | 2 => {
                        if hv != 0x11 {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                    _ => {
                        if self.comp[0].h != h || self.comp[0].v != v {
                            return Err(ERR_UNSUPPORTED_SUBSAMPLING);
                        }
                    }
                },
            }

            self.comp[i].h = h;
            self.comp[i].v = v;
        }
        Ok(())
    }

    /// Port of `processDQT` (reader.go:409).
    fn process_dqt(&mut self, mut n: isize) -> Result<(), JpegError> {
        while n > 0 {
            n -= 1;
            let x = self.read_byte()?;
            let tq = usize::from(x & 0x0f);
            if tq > usize::from(MAX_TQ) {
                return Err(JpegError::Format("bad Tq value"));
            }
            match x >> 4 {
                0 => {
                    if n < BLOCK_SIZE as isize {
                        break;
                    }
                    n -= BLOCK_SIZE as isize;
                    self.read_full_tmp(0, BLOCK_SIZE)?;
                    for i in 0..BLOCK_SIZE {
                        self.quant[tq][i] = i32::from(self.tmp[i]);
                    }
                }
                1 => {
                    if n < 2 * BLOCK_SIZE as isize {
                        break;
                    }
                    n -= 2 * BLOCK_SIZE as isize;
                    self.read_full_tmp(0, 2 * BLOCK_SIZE)?;
                    for i in 0..BLOCK_SIZE {
                        self.quant[tq][i] =
                            i32::from(self.tmp[2 * i]) << 8 | i32::from(self.tmp[2 * i + 1]);
                    }
                }
                _ => return Err(JpegError::Format("bad Pq value")),
            }
        }
        if n != 0 {
            return Err(JpegError::Format("DQT has wrong length"));
        }
        Ok(())
    }

    /// Port of `processDRI` (reader.go:452).
    fn process_dri(&mut self, n: usize) -> Result<(), JpegError> {
        if n != 2 {
            return Err(JpegError::Format("DRI has wrong length"));
        }
        self.read_full_tmp(0, 2)?;
        self.ri = usize::from(self.tmp[0]) << 8 | usize::from(self.tmp[1]);
        Ok(())
    }

    /// Port of `processApp0Marker` (reader.go:463).
    fn process_app0(&mut self, mut n: usize) -> Result<(), JpegError> {
        if n < 5 {
            return self.ignore(n);
        }
        self.read_full_tmp(0, 5)?;
        n -= 5;
        self.jfif = &self.tmp[..5] == b"JFIF\x00";
        if n > 0 {
            return self.ignore(n);
        }
        Ok(())
    }

    /// Port of `processApp14Marker` (reader.go:480).
    fn process_app14(&mut self, mut n: usize) -> Result<(), JpegError> {
        if n < 12 {
            return self.ignore(n);
        }
        self.read_full_tmp(0, 12)?;
        n -= 12;
        if &self.tmp[..5] == b"Adobe" {
            self.adobe_transform_valid = true;
            self.adobe_transform = self.tmp[11];
        }
        if n > 0 {
            return self.ignore(n);
        }
        Ok(())
    }

    /// Port of `decode` (reader.go:500). `Ok(None)` is Go's `(nil, nil)` config-only return.
    fn decode(&mut self, config_only: bool) -> Result<Option<Image>, JpegError> {
        self.read_full_tmp(0, 2)?;
        if self.tmp[0] != 0xff || self.tmp[1] != SOI_MARKER {
            return Err(JpegError::Format("missing SOI marker"));
        }

        loop {
            self.read_full_tmp(0, 2)?;
            while self.tmp[0] != 0xff {
                // Extraneous data before a marker is silently skipped (reader.go:516).
                self.tmp[0] = self.tmp[1];
                self.tmp[1] = self.read_byte()?;
            }
            let mut marker = self.tmp[1];
            if marker == 0 {
                // "\xff\x00" is extraneous data.
                continue;
            }
            while marker == 0xff {
                // Fill bytes (section B.1.1.2).
                marker = self.read_byte()?;
            }
            if marker == EOI_MARKER {
                break;
            }
            if (RST0_MARKER..=RST7_MARKER).contains(&marker) {
                // A stray restart marker after the last ECS is ignored (reader.go:556).
                continue;
            }

            self.read_full_tmp(0, 2)?;
            let n = (isize::from(self.tmp[0]) << 8) + isize::from(self.tmp[1]) - 2;
            if n < 0 {
                return Err(JpegError::Format("short segment length"));
            }
            let nu = n as usize;

            match marker {
                SOF0_MARKER | SOF1_MARKER | SOF2_MARKER => {
                    self.baseline = marker == SOF0_MARKER;
                    self.progressive = marker == SOF2_MARKER;
                    let r = self.process_sof(nu);
                    if config_only && self.jfif {
                        return r.map(|()| None);
                    }
                    r?;
                }
                DHT_MARKER => {
                    if config_only {
                        self.ignore(nu)?;
                    } else {
                        self.process_dht(n)?;
                    }
                }
                DQT_MARKER => {
                    if config_only {
                        self.ignore(nu)?;
                    } else {
                        self.process_dqt(n)?;
                    }
                }
                SOS_MARKER => {
                    if config_only {
                        return Ok(None);
                    }
                    self.process_sos(nu)?;
                }
                DRI_MARKER => {
                    if config_only {
                        self.ignore(nu)?;
                    } else {
                        self.process_dri(nu)?;
                    }
                }
                APP0_MARKER => self.process_app0(nu)?,
                APP14_MARKER => self.process_app14(nu)?,
                _ => {
                    if (APP0_MARKER..=APP15_MARKER).contains(&marker) || marker == COM_MARKER {
                        self.ignore(nu)?;
                    } else if marker < 0xc0 {
                        return Err(JpegError::Format("unknown marker"));
                    } else {
                        return Err(JpegError::Unsupported("unknown marker"));
                    }
                }
            }
        }

        if self.progressive {
            self.reconstruct_progressive_image()?;
        }
        if let Some(img1) = self.img1.take() {
            return Ok(Some(Image::Gray(img1)));
        }
        if let Some(img3) = self.img3.take() {
            if let Some(black) = self.black_pix.take() {
                return self.apply_black(img3, &black).map(Some);
            } else if self.is_rgb() {
                return Ok(Some(self.convert_to_rgb(&img3)));
            }
            return Ok(Some(Image::YCbCr(img3)));
        }
        Err(JpegError::Format("missing SOS marker"))
    }

    /// Port of `applyBlack` (reader.go:661).
    fn apply_black(&self, img3: YCbCr, black: &[u8]) -> Result<Image, JpegError> {
        if !self.adobe_transform_valid {
            return Err(JpegError::Unsupported(
                "unknown color model: 4-component JPEG doesn't have Adobe APP14 metadata",
            ));
        }
        let bounds = img3.rect;
        let (w, h) = (bounds.dx().max(0) as usize, bounds.dy().max(0) as usize);

        if self.adobe_transform != ADOBE_TRANSFORM_UNKNOWN {
            // YCbCrK: YCbCr → RGB (imageutil.DrawYCbCr, which only handles 4:4:4, 4:2:2,
            // 4:2:0 and 4:4:0 — the only ratios a 4-component SOF can produce here), then the
            // inverted K into the fourth byte.
            let mut pix = vec![0u8; w * h * 4];
            let stride = 4 * w;
            let drawable = matches!(
                img3.ratio,
                crate::image::Ratio::R444
                    | crate::image::Ratio::R422
                    | crate::image::Ratio::R420
                    | crate::image::Ratio::R440
            );
            for y in 0..h {
                for x in 0..w {
                    let i = y * stride + 4 * x;
                    if drawable {
                        let yi = img3.y_offset(x as i64, y as i64);
                        let ci = img3.c_offset(x as i64, y as i64);
                        let (r, g, b) =
                            ycbcr_to_rgb(get(&img3.y, yi)?, get(&img3.cb, ci)?, get(&img3.cr, ci)?);
                        pix[i] = r;
                        pix[i + 1] = g;
                        pix[i + 2] = b;
                    }
                    pix[i + 3] = 255 - get(black, y * self.black_stride + x)?;
                }
            }
            return Ok(Image::Cmyk(Pixels {
                pix,
                stride,
                rect: bounds,
            }));
        }

        // CMYK stored inverted: interleave the four planes, undoing the inversion.
        let mut img = Pixels::new(bounds, 4);
        let planes: [(&[u8], usize); 4] = [
            (&img3.y, img3.y_stride),
            (&img3.cb, img3.c_stride),
            (&img3.cr, img3.c_stride),
            (black, self.black_stride),
        ];
        for (t, (src, stride)) in planes.iter().enumerate() {
            let subsample = self.comp[t].h != self.comp[0].h || self.comp[t].v != self.comp[0].v;
            for y in 0..h {
                let sy = if subsample { y / 2 } else { y };
                for x in 0..w {
                    let sx = if subsample { x / 2 } else { x };
                    img.pix[y * img.stride + 4 * x + t] = 255 - get(src, sy * stride + sx)?;
                }
            }
        }
        Ok(Image::Cmyk(img))
    }

    /// Port of `isRGB` (reader.go:730).
    fn is_rgb(&self) -> bool {
        if self.jfif {
            return false;
        }
        if self.adobe_transform_valid && self.adobe_transform == ADOBE_TRANSFORM_UNKNOWN {
            return true;
        }
        self.comp[0].c == b'R' && self.comp[1].c == b'G' && self.comp[2].c == b'B'
    }

    /// Port of `convertToRGB` (reader.go:742).
    fn convert_to_rgb(&self, img3: &YCbCr) -> Image {
        let c_scale = (self.comp[0].h / self.comp[1].h.max(1)).max(1);
        let bounds = img3.rect;
        let mut img = Pixels::new(bounds, 4);
        let w = bounds.dx().max(0) as usize;
        for y in bounds.min_y..bounds.max_y {
            let po = img.offset(bounds.min_x, y, 4);
            let yo = img3.y_offset(bounds.min_x, y);
            let co = img3.c_offset(bounds.min_x, y);
            for i in 0..w {
                img.pix[po + 4 * i] = img3.y.get(yo + i).copied().unwrap_or(0);
                img.pix[po + 4 * i + 1] = img3.cb.get(co + i / c_scale).copied().unwrap_or(0);
                img.pix[po + 4 * i + 2] = img3.cr.get(co + i / c_scale).copied().unwrap_or(0);
                img.pix[po + 4 * i + 3] = 255;
            }
        }
        Image::Rgba(img)
    }
}

/// A bounds-checked read where Go would index (and could only panic on a decoder bug).
fn get(s: &[u8], i: usize) -> Result<u8, JpegError> {
    s.get(i)
        .copied()
        .ok_or(JpegError::Format("sample index out of range"))
}

/// `errMissingFF00` (reader.go:198).
pub(crate) const ERR_MISSING_FF00: JpegError = JpegError::Format("missing 0xff00 sequence");

/// Port of `jpeg.Decode` (reader.go:760).
///
/// Allocates the image (and, for a progressive file, one 256-byte block per 8×8 block of every
/// component) from the SOF dimensions, exactly as Go does — up to 65535×65535. The caller must
/// enforce its resolution limit from [`decode_config`] first, as Mattermost's `imaging.Decoder`
/// does, or a hostile header decides how much memory this takes.
pub fn decode(data: &[u8]) -> Result<Image, JpegError> {
    let mut d = Decoder::new(data);
    match d.decode(false)? {
        Some(img) => Ok(img),
        // `decode(r, false)` never returns (nil, nil).
        None => Err(JpegError::Format("missing SOS marker")),
    }
}

/// Port of `jpeg.DecodeConfig` (reader.go:767).
pub fn decode_config(data: &[u8]) -> Result<Config, JpegError> {
    let mut d = Decoder::new(data);
    d.decode(true)?;
    let (width, height) = (d.width as i64, d.height as i64);
    let model = match d.n_comp {
        1 => ConfigModel::Gray,
        3 => {
            if d.is_rgb() {
                ConfigModel::Rgba
            } else {
                ConfigModel::YCbCr
            }
        }
        4 => ConfigModel::Cmyk,
        _ => return Err(JpegError::Format("missing SOF marker")),
    };
    Ok(Config {
        width,
        height,
        model,
    })
}

/// `image.Rect(0, 0, w, h)`.
pub(crate) fn rect(w: usize, h: usize) -> Rect {
    Rect::new(0, 0, w as i64, h as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refill_keeps_the_last_two_bytes_for_unreading() {
        let data = vec![7u8; 5000];
        let mut d = Decoder::new(&data);
        for _ in 0..4096 {
            d.read_byte().unwrap();
        }
        assert_eq!(d.read_byte().unwrap(), 7);
        assert_eq!((d.bytes.i, d.bytes.j), (3, 2 + 904));
        d.bytes.n_unreadable = 2;
        d.unread_byte_stuffed_byte();
        assert_eq!(d.bytes.i, 1);
    }

    #[test]
    fn end_of_input_is_unexpected_eof_everywhere() {
        assert_eq!(decode(&[]), Err(JpegError::UnexpectedEof));
        assert_eq!(decode(&[0xff]), Err(JpegError::UnexpectedEof));
        assert_eq!(decode(&[0xff, 0xd8]), Err(JpegError::UnexpectedEof));
        assert_eq!(
            decode(&[0xff, 0xd9]),
            Err(JpegError::Format("missing SOI marker"))
        );
    }

    #[test]
    fn eoi_before_any_scan_is_missing_sos_and_config_says_missing_sof() {
        assert_eq!(
            decode(&[0xff, 0xd8, 0xff, 0xd9]),
            Err(JpegError::Format("missing SOS marker"))
        );
        assert_eq!(
            decode_config(&[0xff, 0xd8, 0xff, 0xd9]),
            Err(JpegError::Format("missing SOS marker"))
        );
    }

    #[test]
    fn marker_classes() {
        // 0x01 is below 0xc0: a format error. 0xc3 (lossless SOF) is unsupported.
        assert_eq!(
            decode(&[0xff, 0xd8, 0xff, 0x01, 0x00, 0x02]),
            Err(JpegError::Format("unknown marker"))
        );
        assert_eq!(
            decode(&[0xff, 0xd8, 0xff, 0xc3, 0x00, 0x02]),
            Err(JpegError::Unsupported("unknown marker"))
        );
        assert_eq!(
            decode(&[0xff, 0xd8, 0xff, 0xe5, 0x00, 0x01]),
            Err(JpegError::Format("short segment length"))
        );
        // Extraneous bytes, fill bytes, a stray RST and a comment are all skipped.
        assert_eq!(
            decode(&[
                0xff, 0xd8, 0x12, 0x34, 0xff, 0x00, 0xff, 0xff, 0xd3, 0xff, 0xfe, 0x00, 0x03, 0x41,
                0xff, 0xd9
            ]),
            Err(JpegError::Format("missing SOS marker"))
        );
    }

    #[test]
    fn sof_validation_order() {
        let sof = |body: &[u8]| {
            let mut v = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, (body.len() + 2) as u8];
            v.extend_from_slice(body);
            v.extend_from_slice(&[0xff, 0xd9]);
            decode(&v)
        };
        assert_eq!(
            sof(&[8, 0, 1, 0, 1]),
            Err(JpegError::Unsupported("number of components"))
        );
        assert_eq!(
            sof(&[12, 0, 1, 0, 1, 1, 1, 0x11, 0]),
            Err(JpegError::Unsupported("precision"))
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 2, 1, 0x11, 0]),
            Err(JpegError::Format("SOF has wrong length"))
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 1, 1, 0x11, 4]),
            Err(JpegError::Format("bad Tq value"))
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 1, 1, 0x51, 0]),
            Err(JpegError::Format("luma/chroma subsampling ratio"))
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 1, 1, 0x31, 0]),
            Err(ERR_UNSUPPORTED_SUBSAMPLING)
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 3, 1, 0x11, 0, 1, 0x11, 0, 3, 0x11, 0]),
            Err(JpegError::Format("repeated component identifier"))
        );
        // Y (1,4) is rejected; Cb must divide Y; Cr must equal Cb.
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 3, 1, 0x14, 0, 2, 0x11, 0, 3, 0x11, 0]),
            Err(ERR_UNSUPPORTED_SUBSAMPLING)
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 3, 1, 0x21, 0, 2, 0x41, 0, 3, 0x41, 0]),
            Err(ERR_UNSUPPORTED_SUBSAMPLING)
        );
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x21, 0]),
            Err(ERR_UNSUPPORTED_SUBSAMPLING)
        );
        // A valid SOF then EOI with no scan.
        assert_eq!(
            sof(&[8, 0, 1, 0, 1, 1, 1, 0x22, 0]),
            Err(JpegError::Format("missing SOS marker"))
        );
    }

    #[test]
    fn dqt_and_dri_lengths() {
        let seg = |marker: u8, body: &[u8]| {
            let mut v = vec![0xff, 0xd8, 0xff, marker, 0x00, (body.len() + 2) as u8];
            v.extend_from_slice(body);
            v.extend_from_slice(&[0xff, 0xd9]);
            decode(&v)
        };
        assert_eq!(seg(0xdb, &[0x04]), Err(JpegError::Format("bad Tq value")));
        assert_eq!(seg(0xdb, &[0x20]), Err(JpegError::Format("bad Pq value")));
        // A table too short for its precision stops the loop with bytes left: wrong length.
        assert_eq!(
            seg(0xdb, &[0x00, 1, 2]),
            Err(JpegError::Format("DQT has wrong length"))
        );
        assert_eq!(
            seg(0xdd, &[0, 1, 2]),
            Err(JpegError::Format("DRI has wrong length"))
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::{decode, decode_config};
    use crate::testsupport::{b64, describe, fixture};
    use serde_json::{Value as Json, json};

    /// Every case of the oracle's JPEG decode corpus: GOROOT's testdata (every subsampling
    /// ratio, progressive, restart markers, CMYK, Adobe RGB, truncation), Go-encoded files, and
    /// damaged copies. Inputs whose first two bytes are not the JPEG magic never reach the JPEG
    /// decoder in Go — `image.Decode` answers "unknown format" — so they are counted and skipped.
    #[test]
    fn every_decode_case_matches_go() {
        let cases = fixture("jpeg")["decode"].as_array().unwrap();
        let (mut checked, mut skipped) = (0, 0);
        let mut failures = Vec::new();
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            if !data.starts_with(&[0xff, 0xd8]) {
                skipped += 1;
                continue;
            }
            checked += 1;
            let config = match decode_config(&data) {
                Ok(cfg) => {
                    json!({"w": cfg.width, "h": cfg.height, "format": "jpeg", "model": cfg.model.name()})
                }
                Err(e) => json!({"err": e.to_string()}),
            };
            if config != c["config"] {
                failures.push(format!("{name} config: {config} vs Go {}", c["config"]));
            }
            let image = match decode(&data) {
                Ok(img) => {
                    let mut d = describe(&img);
                    d["format"] = Json::from("jpeg");
                    d
                }
                Err(e) => json!({"err": e.to_string()}),
            };
            if image != c["image"] {
                failures.push(format!("{name} image: {image} vs Go {}", c["image"]));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {checked}:\n{}",
            failures.len(),
            failures.join("\n")
        );
        assert!(checked > 60, "{checked}");
        eprintln!("jpeg decode: {checked} checked, {skipped} skipped (not JPEG magic)");
    }
}
