//! Port of Go's PNG decoder (image/png/reader.go, go1.26.4): `png.Decode` and `png.DecodeConfig`
//! as `image.Decode`/`image.DecodeConfig` call them.
//!
//! # The reader chain is Go's, layer by layer
//!
//! `image.Decode` hands the decoder a `bufio.Reader` over the caller's `bytes.Reader` (the source
//! has no `Peek`), and `zlib.NewReader` puts a second `bufio.Reader` in front of the decoder's IDAT
//! reader (which has no `ReadByte`). Both buffers read ahead, and how far is observable: after the
//! last row, the decoder demands the zlib stream end *and* the IDAT chunk be exhausted, so trailing
//! bytes the zlib buffer already swallowed pass while trailing bytes it did not are "too much pixel
//! data". Both buffers are therefore [`BufReader`]s with Go's read pattern, not a slice cursor.
//!
//! # Allocation
//!
//! Like Go, the decoder allocates the whole image from IHDR's dimensions when it reaches the first
//! IDAT. It checks only what Go checks (positive, `w*h*8` fits); the resolution limit is the
//! caller's job, exactly as Mattermost's `imaging.Decoder` runs `DecodeConfig` first.

use super::paeth::filter_paeth;
use crate::goread::{BufReader, BytesReader, Error, Read, read_full, read_full_err};
use crate::hash::Crc32;
use crate::image::{Color, Image, Paletted, Pixels, Rect};
use crate::zlib::reader::Reader as ZlibReader;

// Color type, as per the PNG spec.
const CT_GRAYSCALE: u8 = 0;
const CT_TRUE_COLOR: u8 = 2;
const CT_PALETTED: u8 = 3;
const CT_GRAYSCALE_ALPHA: u8 = 4;
const CT_TRUE_COLOR_ALPHA: u8 = 6;

/// A `cb`: a combination of colour type and bit depth (reader.go:31).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cb {
    Invalid,
    G1,
    G2,
    G4,
    G8,
    GA8,
    TC8,
    P1,
    P2,
    P4,
    P8,
    TCA8,
    G16,
    GA16,
    TC16,
    TCA16,
}

impl Cb {
    fn paletted(self) -> bool {
        matches!(self, Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8)
    }

    fn true_color(self) -> bool {
        matches!(self, Cb::TC8 | Cb::TC16)
    }
}

// Filter type, as per the PNG spec.
const FT_NONE: u8 = 0;
const FT_SUB: u8 = 1;
const FT_UP: u8 = 2;
const FT_AVERAGE: u8 = 3;
const FT_PAETH: u8 = 4;

const IT_NONE: u8 = 0;
const IT_ADAM7: u8 = 1;

/// `interlacing` (reader.go:84): xFactor, yFactor, xOffset, yOffset per Adam7 pass.
const INTERLACING: [(usize, usize, usize, usize); 7] = [
    (8, 8, 0, 0),
    (8, 8, 4, 0),
    (4, 8, 0, 4),
    (4, 4, 2, 0),
    (2, 4, 0, 2),
    (2, 2, 1, 0),
    (1, 2, 0, 1),
];

// Decoding stage (reader.go:99).
const DS_START: u8 = 0;
const DS_SEEN_IHDR: u8 = 1;
const DS_SEEN_PLTE: u8 = 2;
const DS_SEEN_TRNS: u8 = 3;
const DS_SEEN_IDAT: u8 = 4;
const DS_SEEN_IEND: u8 = 5;

const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n";

fn format(s: &str) -> Error {
    Error::PngFormat(s.to_owned())
}

fn chunk_order_error() -> Error {
    format("chunk out of order")
}

/// The colour model `png.DecodeConfig` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigModel {
    Gray,
    Gray16,
    Rgba,
    Rgba64,
    Nrgba,
    Nrgba64,
    /// `d.palette`: the PLTE entries (as `color.RGBA`), extended and retyped by tRNS.
    Palette(Vec<Color>),
}

/// `image.Config` for a PNG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: ConfigModel,
}

/// Everything `readImagePass` needs from the decoder, copied out so the IDAT reader can hold the
/// decoder mutably while the passes are read.
struct Params {
    cb: Cb,
    depth: usize,
    width: usize,
    height: usize,
    interlace: u8,
    use_transparent: bool,
    transparent: [u8; 6],
    /// The 256-entry backing array of `d.palette`; Go's slice may grow into it.
    palette_backing: Vec<Color>,
    palette_len: usize,
}

/// Port of `decoder` (reader.go:109).
struct Decoder<'a> {
    r: BufReader<BytesReader<'a>>,
    img: Option<Image>,
    crc: Crc32,
    width: usize,
    height: usize,
    depth: usize,
    /// `d.palette`'s backing array (256 entries once PLTE is seen) and its length.
    palette_backing: Vec<Color>,
    palette_len: usize,
    has_palette: bool,
    cb: Cb,
    stage: u8,
    idat_length: u32,
    tmp: [u8; 3 * 256],
    interlace: u8,
    use_transparent: bool,
    transparent: [u8; 6],
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Decoder {
            r: BufReader::new(BytesReader::new(data)),
            img: None,
            crc: Crc32::new(),
            width: 0,
            height: 0,
            depth: 0,
            palette_backing: Vec::new(),
            palette_len: 0,
            has_palette: false,
            cb: Cb::Invalid,
            stage: DS_START,
            idat_length: 0,
            tmp: [0; 3 * 256],
            interlace: 0,
            use_transparent: false,
            transparent: [0; 6],
        }
    }

    /// `parseIHDR` (reader.go:137).
    fn parse_ihdr(&mut self, length: u32) -> Result<(), Error> {
        if length != 13 {
            return Err(format("bad IHDR length"));
        }
        read_full_err(&mut self.r, &mut self.tmp[..13])?;
        self.crc.update(&self.tmp[..13]);
        if self.tmp[10] != 0 {
            return Err(Error::PngUnsupported("compression method".into()));
        }
        if self.tmp[11] != 0 {
            return Err(Error::PngUnsupported("filter method".into()));
        }
        if self.tmp[12] != IT_NONE && self.tmp[12] != IT_ADAM7 {
            return Err(format("invalid interlace method"));
        }
        self.interlace = self.tmp[12];

        let w = i32::from_be_bytes([self.tmp[0], self.tmp[1], self.tmp[2], self.tmp[3]]);
        let h = i32::from_be_bytes([self.tmp[4], self.tmp[5], self.tmp[6], self.tmp[7]]);
        if w <= 0 || h <= 0 {
            return Err(format("non-positive dimension"));
        }
        // On a 64-bit Go `int`, `w*h` always fits and `w*h*8` does too (< 2^65 would not, but
        // both factors are < 2^31, so the product is < 2^62 and times 8 is < 2^65 — Go's check
        // `nPixels != (nPixels*8)/8` fails only past 2^60).
        let n_pixels = i64::from(w) * i64::from(h);
        if n_pixels.checked_mul(8).is_none() {
            return Err(Error::PngUnsupported("dimension overflow".into()));
        }

        self.cb = Cb::Invalid;
        self.depth = usize::from(self.tmp[8]);
        let ct = self.tmp[9];
        self.cb = match (self.depth, ct) {
            (1, CT_GRAYSCALE) => Cb::G1,
            (1, CT_PALETTED) => Cb::P1,
            (2, CT_GRAYSCALE) => Cb::G2,
            (2, CT_PALETTED) => Cb::P2,
            (4, CT_GRAYSCALE) => Cb::G4,
            (4, CT_PALETTED) => Cb::P4,
            (8, CT_GRAYSCALE) => Cb::G8,
            (8, CT_TRUE_COLOR) => Cb::TC8,
            (8, CT_PALETTED) => Cb::P8,
            (8, CT_GRAYSCALE_ALPHA) => Cb::GA8,
            (8, CT_TRUE_COLOR_ALPHA) => Cb::TCA8,
            (16, CT_GRAYSCALE) => Cb::G16,
            (16, CT_TRUE_COLOR) => Cb::TC16,
            (16, CT_GRAYSCALE_ALPHA) => Cb::GA16,
            (16, CT_TRUE_COLOR_ALPHA) => Cb::TCA16,
            _ => Cb::Invalid,
        };
        if self.cb == Cb::Invalid {
            return Err(Error::PngUnsupported(format!(
                "bit depth {}, color type {}",
                self.tmp[8], self.tmp[9]
            )));
        }
        self.width = w as usize;
        self.height = h as usize;
        self.verify_checksum()
    }

    /// `parsePLTE` (reader.go:212).
    fn parse_plte(&mut self, length: u32) -> Result<(), Error> {
        let np = (length / 3) as usize;
        if length % 3 != 0 || np == 0 || np > 256 || np > 1usize << self.depth.min(16) {
            return Err(format("bad PLTE length"));
        }
        let (n, err) = read_full(&mut self.r, &mut self.tmp[..3 * np]);
        if let Some(e) = err {
            return Err(e);
        }
        self.crc.update(&self.tmp[..n]);
        match self.cb {
            Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => {
                let mut pal = vec![Color::Rgba([0, 0, 0, 0xff]); 256];
                for (i, c) in pal.iter_mut().enumerate().take(np) {
                    *c = Color::Rgba([
                        self.tmp[3 * i],
                        self.tmp[3 * i + 1],
                        self.tmp[3 * i + 2],
                        0xff,
                    ]);
                }
                self.palette_backing = pal;
                self.palette_len = np;
                self.has_palette = true;
            }
            Cb::TC8 | Cb::TCA8 | Cb::TC16 | Cb::TCA16 => {
                // A PLTE chunk is optional, and ignorable, for truecolour (section 4.1.2).
            }
            _ => return Err(format("PLTE, color type mismatch")),
        }
        self.verify_checksum()
    }

    /// `parsetRNS` (reader.go:242).
    fn parse_trns(&mut self, length: u32) -> Result<(), Error> {
        match self.cb {
            Cb::G1 | Cb::G2 | Cb::G4 | Cb::G8 | Cb::G16 => {
                if length != 2 {
                    return Err(format("bad tRNS length"));
                }
                let (n, err) = read_full(&mut self.r, &mut self.tmp[..2]);
                if let Some(e) = err {
                    return Err(e);
                }
                self.crc.update(&self.tmp[..n]);
                self.transparent[..2].copy_from_slice(&self.tmp[..2]);
                match self.cb {
                    Cb::G1 => self.transparent[1] = self.transparent[1].wrapping_mul(0xff),
                    Cb::G2 => self.transparent[1] = self.transparent[1].wrapping_mul(0x55),
                    Cb::G4 => self.transparent[1] = self.transparent[1].wrapping_mul(0x11),
                    _ => {}
                }
                self.use_transparent = true;
            }
            Cb::TC8 | Cb::TC16 => {
                if length != 6 {
                    return Err(format("bad tRNS length"));
                }
                let (n, err) = read_full(&mut self.r, &mut self.tmp[..6]);
                if let Some(e) = err {
                    return Err(e);
                }
                self.crc.update(&self.tmp[..n]);
                self.transparent.copy_from_slice(&self.tmp[..6]);
                self.use_transparent = true;
            }
            Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => {
                if length > 256 {
                    return Err(format("bad tRNS length"));
                }
                let (n, err) = read_full(&mut self.r, &mut self.tmp[..length as usize]);
                if let Some(e) = err {
                    return Err(e);
                }
                self.crc.update(&self.tmp[..n]);
                if self.palette_len < n {
                    self.palette_len = n;
                }
                for i in 0..n {
                    // `d.palette[i].(color.RGBA)`: always an RGBA here, since tRNS can follow PLTE
                    // only once.
                    if let Some(Color::Rgba([r, g, b, _])) = self.palette_backing.get(i).copied() {
                        self.palette_backing[i] = Color::Nrgba([r, g, b, self.tmp[i]]);
                    }
                }
            }
            _ => return Err(format("tRNS, color type mismatch")),
        }
        self.verify_checksum()
    }

    /// `decode` (reader.go:337): the IDAT stream through zlib into an image.
    fn decode(&mut self) -> Result<Image, Error> {
        let params = Params {
            cb: self.cb,
            depth: self.depth,
            width: self.width,
            height: self.height,
            interlace: self.interlace,
            use_transparent: self.use_transparent,
            transparent: self.transparent,
            palette_backing: self.palette_backing.clone(),
            palette_len: self.palette_len,
        };
        let mut r = ZlibReader::new(BufReader::new(IdatReader { d: self }))?;
        let img = if params.interlace == IT_NONE {
            read_image_pass(&params, Some(&mut r), 0, false)?
        } else {
            let mut img = read_image_pass::<BytesReader>(&params, None, 0, true)?;
            for pass in 0..7 {
                if let (Some(pass_img), Some(dst)) = (
                    read_image_pass(&params, Some(&mut r), pass, false)?,
                    img.as_mut(),
                ) {
                    merge_pass_into(dst, &pass_img, pass);
                }
            }
            img
        };

        // Check for EOF, to verify the zlib checksum.
        let mut n = 0;
        let mut err = None;
        let mut tmp = [0u8; 1];
        let mut i = 0;
        while n == 0 && err.is_none() {
            if i == 100 {
                return Err(Error::NoProgress);
            }
            (n, err) = r.read(&mut tmp);
            i += 1;
        }
        if let Some(e) = err
            && e != Error::Eof
        {
            return Err(Error::PngFormat(e.to_string()));
        }
        let idat_length = r.get_mut().get_mut().d.idat_length;
        if n != 0 || idat_length != 0 {
            return Err(format("too much pixel data"));
        }
        img.ok_or_else(|| format("no image"))
    }

    /// `parseIDAT` (reader.go:873).
    fn parse_idat(&mut self, length: u32) -> Result<(), Error> {
        self.idat_length = length;
        self.img = Some(self.decode()?);
        self.verify_checksum()
    }

    /// `parseIEND` (reader.go:882).
    fn parse_iend(&mut self, length: u32) -> Result<(), Error> {
        if length != 0 {
            return Err(format("bad IEND length"));
        }
        self.verify_checksum()
    }

    /// `parseChunk` (reader.go:889).
    fn parse_chunk(&mut self, config_only: bool) -> Result<(), Error> {
        read_full_err(&mut self.r, &mut self.tmp[..8])?;
        let length = u32::from_be_bytes([self.tmp[0], self.tmp[1], self.tmp[2], self.tmp[3]]);
        self.crc = Crc32::new();
        let kind = [self.tmp[4], self.tmp[5], self.tmp[6], self.tmp[7]];
        self.crc.update(&kind);

        match &kind {
            b"IHDR" => {
                if self.stage != DS_START {
                    return Err(chunk_order_error());
                }
                self.stage = DS_SEEN_IHDR;
                return self.parse_ihdr(length);
            }
            b"PLTE" => {
                if self.stage != DS_SEEN_IHDR {
                    return Err(chunk_order_error());
                }
                self.stage = DS_SEEN_PLTE;
                return self.parse_plte(length);
            }
            b"tRNS" => {
                if self.cb.paletted() {
                    if self.stage != DS_SEEN_PLTE {
                        return Err(chunk_order_error());
                    }
                } else if self.cb.true_color() {
                    if self.stage != DS_SEEN_IHDR && self.stage != DS_SEEN_PLTE {
                        return Err(chunk_order_error());
                    }
                } else if self.stage != DS_SEEN_IHDR {
                    return Err(chunk_order_error());
                }
                self.stage = DS_SEEN_TRNS;
                return self.parse_trns(length);
            }
            b"IDAT" => {
                if self.stage < DS_SEEN_IHDR
                    || self.stage > DS_SEEN_IDAT
                    || (self.stage == DS_SEEN_IHDR && self.cb.paletted())
                {
                    return Err(chunk_order_error());
                } else if self.stage != DS_SEEN_IDAT {
                    self.stage = DS_SEEN_IDAT;
                    if config_only {
                        return Ok(());
                    }
                    return self.parse_idat(length);
                }
                // Trailing zero-length or garbage IDAT chunks are ignored below.
            }
            b"IEND" => {
                if self.stage != DS_SEEN_IDAT {
                    return Err(chunk_order_error());
                }
                self.stage = DS_SEEN_IEND;
                return self.parse_iend(length);
            }
            _ => {}
        }
        if length > 0x7fff_ffff {
            return Err(Error::PngFormat(format!("Bad chunk length: {length}")));
        }
        // Ignore this chunk (of a known length).
        let mut ignored = [0u8; 4096];
        let mut length = length as usize;
        while length > 0 {
            let want = ignored.len().min(length);
            let (n, err) = read_full(&mut self.r, &mut ignored[..want]);
            if let Some(e) = err {
                return Err(e);
            }
            self.crc.update(&ignored[..n]);
            length -= n;
        }
        self.verify_checksum()
    }

    /// `verifyChecksum` (reader.go:966).
    fn verify_checksum(&mut self) -> Result<(), Error> {
        read_full_err(&mut self.r, &mut self.tmp[..4])?;
        if u32::from_be_bytes([self.tmp[0], self.tmp[1], self.tmp[2], self.tmp[3]])
            != self.crc.sum()
        {
            return Err(format("invalid checksum"));
        }
        Ok(())
    }

    /// `checkHeader` (reader.go:976).
    fn check_header(&mut self) -> Result<(), Error> {
        read_full_err(&mut self.r, &mut self.tmp[..PNG_HEADER.len()])?;
        if &self.tmp[..PNG_HEADER.len()] != PNG_HEADER {
            return Err(format("not a PNG file"));
        }
        Ok(())
    }
}

/// `io.EOF` → `io.ErrUnexpectedEOF`, as `Decode` and `DecodeConfig` map a chunk-level error.
fn eof_to_unexpected(e: Error) -> Error {
    if e == Error::Eof {
        Error::UnexpectedEof
    } else {
        e
    }
}

/// Port of `png.Decode` (reader.go:991) as `image.Decode` calls it: over a `bufio.Reader` on the
/// whole input.
///
/// Allocates the full image from the IHDR dimensions: callers enforce a resolution limit first
/// (Mattermost's `imaging.Decoder` does, via `DecodeConfig`).
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    let mut d = Decoder::new(data);
    d.check_header().map_err(eof_to_unexpected)?;
    while d.stage != DS_SEEN_IEND {
        d.parse_chunk(false).map_err(eof_to_unexpected)?;
    }
    d.img.ok_or_else(|| format("no image"))
}

/// Port of `png.DecodeConfig` (reader.go:1012) as `image.DecodeConfig` calls it.
pub fn decode_config(data: &[u8]) -> Result<Config, Error> {
    let mut d = Decoder::new(data);
    d.check_header().map_err(eof_to_unexpected)?;
    loop {
        d.parse_chunk(true).map_err(eof_to_unexpected)?;
        if d.cb.paletted() {
            if d.stage >= DS_SEEN_TRNS {
                break;
            }
        } else if d.stage >= DS_SEEN_IHDR {
            break;
        }
    }
    let model = match d.cb {
        Cb::G1 | Cb::G2 | Cb::G4 | Cb::G8 => ConfigModel::Gray,
        Cb::GA8 | Cb::TCA8 => ConfigModel::Nrgba,
        Cb::TC8 => ConfigModel::Rgba,
        Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => ConfigModel::Palette(
            d.palette_backing
                .get(..d.palette_len)
                .unwrap_or(&[])
                .to_vec(),
        ),
        Cb::G16 => ConfigModel::Gray16,
        Cb::GA16 | Cb::TCA16 => ConfigModel::Nrgba64,
        Cb::TC16 => ConfigModel::Rgba64,
        // Unreachable: parseIHDR rejects an invalid cb before the loop can break.
        Cb::Invalid => return Err(Error::PngUnsupported("bit depth, color type".into())),
    };
    Ok(Config {
        width: d.width as i64,
        height: d.height as i64,
        model,
    })
}

/// The decoder as an `io.Reader` over its IDAT chunks (reader.go:302, `decoder.Read`).
struct IdatReader<'d, 'a> {
    d: &'d mut Decoder<'a>,
}

impl Read for IdatReader<'_, '_> {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if p.is_empty() {
            return (0, None);
        }
        let d = &mut *self.d;
        while d.idat_length == 0 {
            // An IDAT chunk is exhausted: verify its checksum, then require another IDAT.
            if let Err(e) = d.verify_checksum() {
                return (0, Some(e));
            }
            if let (_, Some(e)) = read_full(&mut d.r, &mut d.tmp[..8]) {
                return (0, Some(e));
            }
            d.idat_length = u32::from_be_bytes([d.tmp[0], d.tmp[1], d.tmp[2], d.tmp[3]]);
            if &d.tmp[4..8] != b"IDAT" {
                return (0, Some(format("not enough pixel data")));
            }
            d.crc = Crc32::new();
            let kind = [d.tmp[4], d.tmp[5], d.tmp[6], d.tmp[7]];
            d.crc.update(&kind);
        }
        // `int(d.idatLength) < 0` cannot hold for a uint32 on a 64-bit `int`.
        let want = p.len().min(d.idat_length as usize);
        let (n, err) = d.r.read(&mut p[..want]);
        d.crc.update(&p[..n]);
        d.idat_length -= n as u32;
        (n, err)
    }
}

fn new_pixels(w: usize, h: usize, bpp: usize) -> Pixels {
    Pixels::new(Rect::new(0, 0, w as i64, h as i64), bpp)
}

/// Port of `readImagePass` (reader.go:410). `r` is `None` exactly when `allocate_only`.
fn read_image_pass<R: Read>(
    p: &Params,
    r: Option<&mut R>,
    pass: usize,
    allocate_only: bool,
) -> Result<Option<Image>, Error> {
    let (mut width, mut height) = (p.width, p.height);
    if p.interlace == IT_ADAM7 && !allocate_only {
        let (xf, yf, xo, yo) = INTERLACING[pass];
        width = (width + xf - 1).saturating_sub(xo) / xf;
        height = (height + yf - 1).saturating_sub(yo) / yf;
        if width == 0 || height == 0 {
            return Ok(None);
        }
    }
    let (bits_per_pixel, mut img) = match p.cb {
        Cb::G1 | Cb::G2 | Cb::G4 | Cb::G8 => (
            p.depth,
            if p.use_transparent {
                Image::Nrgba(new_pixels(width, height, 4))
            } else {
                Image::Gray(new_pixels(width, height, 1))
            },
        ),
        Cb::GA8 => (16, Image::Nrgba(new_pixels(width, height, 4))),
        Cb::TC8 => (
            24,
            if p.use_transparent {
                Image::Nrgba(new_pixels(width, height, 4))
            } else {
                Image::Rgba(new_pixels(width, height, 4))
            },
        ),
        Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => (
            p.depth,
            Image::Paletted(Paletted {
                pix: new_pixels(width, height, 1),
                palette: p
                    .palette_backing
                    .get(..p.palette_len)
                    .unwrap_or(&[])
                    .to_vec(),
            }),
        ),
        Cb::TCA8 => (32, Image::Nrgba(new_pixels(width, height, 4))),
        Cb::G16 => (
            16,
            if p.use_transparent {
                Image::Nrgba64(new_pixels(width, height, 8))
            } else {
                Image::Gray16(new_pixels(width, height, 2))
            },
        ),
        Cb::GA16 => (32, Image::Nrgba64(new_pixels(width, height, 8))),
        Cb::TC16 => (
            48,
            if p.use_transparent {
                Image::Nrgba64(new_pixels(width, height, 8))
            } else {
                Image::Rgba64(new_pixels(width, height, 8))
            },
        ),
        Cb::TCA16 => (64, Image::Nrgba64(new_pixels(width, height, 8))),
        Cb::Invalid => return Ok(None),
    };
    let Some(r) = r else {
        return Ok(Some(img));
    };
    if allocate_only {
        return Ok(Some(img));
    }
    let bytes_per_pixel = bits_per_pixel.div_ceil(8);
    let row_size = 1 + (bits_per_pixel * width).div_ceil(8);
    let mut cr = vec![0u8; row_size];
    let mut pr = vec![0u8; row_size];
    let mut pix_offset = 0usize;

    for y in 0..height {
        let (_, err) = read_full(r, &mut cr);
        if let Some(e) = err {
            if e == Error::Eof || e == Error::UnexpectedEof {
                return Err(format("not enough pixel data"));
            }
            return Err(e);
        }

        // Apply the filter.
        let (head, cdat) = cr.split_at_mut(1);
        let pdat = &pr[1..];
        match head[0] {
            FT_NONE => {}
            FT_SUB => {
                for i in bytes_per_pixel..cdat.len() {
                    cdat[i] = cdat[i].wrapping_add(cdat[i - bytes_per_pixel]);
                }
            }
            FT_UP => {
                for (c, &q) in cdat.iter_mut().zip(pdat) {
                    *c = c.wrapping_add(q);
                }
            }
            FT_AVERAGE => {
                for i in 0..bytes_per_pixel.min(cdat.len()) {
                    cdat[i] = cdat[i].wrapping_add(pdat[i] / 2);
                }
                for i in bytes_per_pixel..cdat.len() {
                    let avg = (usize::from(cdat[i - bytes_per_pixel]) + usize::from(pdat[i])) / 2;
                    cdat[i] = cdat[i].wrapping_add(avg as u8);
                }
            }
            FT_PAETH => filter_paeth(cdat, pdat, bytes_per_pixel),
            _ => return Err(format("bad filter type")),
        }

        convert_row(p, &mut img, cdat, y, width, &mut pix_offset);

        // The current row for y is the previous row for y+1.
        std::mem::swap(&mut pr, &mut cr);
    }
    Ok(Some(img))
}

/// `SetNRGBA`/`SetNRGBA64`/… at (x, y) of a freshly allocated, origin-anchored image.
fn put(pix: &mut Pixels, x: usize, y: usize, bytes: &[u8]) {
    let i = y * pix.stride + x * bytes.len();
    if let Some(dst) = pix.pix.get_mut(i..i + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

/// `copy(dst[off:], src)`.
fn copy_at(dst: &mut [u8], off: usize, src: &[u8]) {
    if let Some(d) = dst.get_mut(off..) {
        let n = d.len().min(src.len());
        d[..n].copy_from_slice(&src[..n]);
    }
}

/// The "Convert from bytes to colors" switch of `readImagePass` (reader.go:537).
fn convert_row(
    p: &Params,
    img: &mut Image,
    cdat: &[u8],
    y: usize,
    width: usize,
    pix_offset: &mut usize,
) {
    match (p.cb, img) {
        (Cb::G1 | Cb::G2 | Cb::G4, img) => {
            let (bits, scale) = match p.cb {
                Cb::G1 => (1usize, 0xffu8),
                Cb::G2 => (2, 0x55),
                _ => (4, 0x11),
            };
            let per_byte = 8 / bits;
            let ty = p.transparent[1];
            let mut x = 0;
            while x < width {
                let mut b = cdat[x / per_byte];
                let mut x2 = 0;
                while x2 < per_byte && x + x2 < width {
                    let ycol = (b >> (8 - bits)).wrapping_mul(scale);
                    match img {
                        Image::Nrgba(m) => {
                            let a = if ycol == ty { 0 } else { 0xff };
                            put(m, x + x2, y, &[ycol, ycol, ycol, a]);
                        }
                        Image::Gray(m) => put(m, x + x2, y, &[ycol]),
                        _ => {}
                    }
                    b <<= bits;
                    x2 += 1;
                }
                x += per_byte;
            }
        }
        (Cb::G8, Image::Nrgba(m)) => {
            let ty = p.transparent[1];
            for (x, &ycol) in cdat.iter().enumerate().take(width) {
                let a = if ycol == ty { 0 } else { 0xff };
                put(m, x, y, &[ycol, ycol, ycol, a]);
            }
        }
        (Cb::G8, Image::Gray(m)) => {
            copy_at(&mut m.pix, *pix_offset, cdat);
            *pix_offset += m.stride;
        }
        (Cb::GA8, Image::Nrgba(m)) => {
            for x in 0..width {
                let ycol = cdat[2 * x];
                put(m, x, y, &[ycol, ycol, ycol, cdat[2 * x + 1]]);
            }
        }
        (Cb::TC8, Image::Nrgba(m)) => {
            let (tr, tg, tb) = (p.transparent[1], p.transparent[3], p.transparent[5]);
            let mut i = *pix_offset;
            for px in cdat.chunks_exact(3).take(width) {
                let a = if px[0] == tr && px[1] == tg && px[2] == tb {
                    0
                } else {
                    0xff
                };
                copy_at(&mut m.pix, i, &[px[0], px[1], px[2], a]);
                i += 4;
            }
            *pix_offset += m.stride;
        }
        (Cb::TC8, Image::Rgba(m)) => {
            let mut i = *pix_offset;
            for px in cdat.chunks_exact(3).take(width) {
                copy_at(&mut m.pix, i, &[px[0], px[1], px[2], 0xff]);
                i += 4;
            }
            *pix_offset += m.stride;
        }
        (Cb::P1 | Cb::P2 | Cb::P4, Image::Paletted(m)) => {
            let bits = match p.cb {
                Cb::P1 => 1usize,
                Cb::P2 => 2,
                _ => 4,
            };
            let per_byte = 8 / bits;
            let mut x = 0;
            while x < width {
                let mut b = cdat[x / per_byte];
                let mut x2 = 0;
                while x2 < per_byte && x + x2 < width {
                    let idx = b >> (8 - bits);
                    extend_palette(p, &mut m.palette, idx);
                    put(&mut m.pix, x + x2, y, &[idx]);
                    b <<= bits;
                    x2 += 1;
                }
                x += per_byte;
            }
        }
        (Cb::P8, Image::Paletted(m)) => {
            if m.palette.len() != 256 {
                for &idx in cdat.iter().take(width) {
                    extend_palette(p, &mut m.palette, idx);
                }
            }
            copy_at(&mut m.pix.pix, *pix_offset, cdat);
            *pix_offset += m.pix.stride;
        }
        (Cb::TCA8, Image::Nrgba(m)) => {
            copy_at(&mut m.pix, *pix_offset, cdat);
            *pix_offset += m.stride;
        }
        (Cb::G16, Image::Nrgba64(m)) => {
            let (t0, t1) = (p.transparent[0], p.transparent[1]);
            for (x, px) in cdat.chunks_exact(2).take(width).enumerate() {
                let a = if px[0] == t0 && px[1] == t1 {
                    [0, 0]
                } else {
                    [0xff, 0xff]
                };
                put(
                    m,
                    x,
                    y,
                    &[px[0], px[1], px[0], px[1], px[0], px[1], a[0], a[1]],
                );
            }
        }
        (Cb::G16, Image::Gray16(m)) => {
            for (x, px) in cdat.chunks_exact(2).take(width).enumerate() {
                put(m, x, y, px);
            }
        }
        (Cb::GA16, Image::Nrgba64(m)) => {
            for (x, px) in cdat.chunks_exact(4).take(width).enumerate() {
                put(
                    m,
                    x,
                    y,
                    &[px[0], px[1], px[0], px[1], px[0], px[1], px[2], px[3]],
                );
            }
        }
        (Cb::TC16, Image::Nrgba64(m)) => {
            let t = p.transparent;
            for (x, px) in cdat.chunks_exact(6).take(width).enumerate() {
                let a = if px == t { [0, 0] } else { [0xff, 0xff] };
                put(
                    m,
                    x,
                    y,
                    &[px[0], px[1], px[2], px[3], px[4], px[5], a[0], a[1]],
                );
            }
        }
        (Cb::TC16, Image::Rgba64(m)) => {
            for (x, px) in cdat.chunks_exact(6).take(width).enumerate() {
                put(
                    m,
                    x,
                    y,
                    &[px[0], px[1], px[2], px[3], px[4], px[5], 0xff, 0xff],
                );
            }
        }
        (Cb::TCA16, Image::Nrgba64(m)) => {
            for (x, px) in cdat.chunks_exact(8).take(width).enumerate() {
                put(m, x, y, px);
            }
        }
        _ => {}
    }
}

/// `if len(paletted.Palette) <= int(idx) { paletted.Palette = paletted.Palette[:int(idx)+1] }` —
/// growing into the 256-entry backing array PLTE allocated.
fn extend_palette(p: &Params, palette: &mut Vec<Color>, idx: u8) {
    let idx = usize::from(idx);
    if palette.len() <= idx {
        if let Some(more) = p.palette_backing.get(palette.len()..=idx) {
            palette.extend_from_slice(more);
        }
    }
}

/// Port of `mergePassInto` (reader.go:791).
fn merge_pass_into(dst: &mut Image, src: &Image, pass: usize) {
    let (xf, yf, xo, yo) = INTERLACING[pass];
    let (src_pix, dst_pix, stride, bpp) = match (dst, src) {
        (Image::Gray(d), Image::Gray(s)) => (&s.pix, &mut d.pix, d.stride, 1),
        (Image::Gray16(d), Image::Gray16(s)) => (&s.pix, &mut d.pix, d.stride, 2),
        (Image::Nrgba(d), Image::Nrgba(s)) => (&s.pix, &mut d.pix, d.stride, 4),
        (Image::Nrgba64(d), Image::Nrgba64(s)) => (&s.pix, &mut d.pix, d.stride, 8),
        (Image::Rgba(d), Image::Rgba(s)) => (&s.pix, &mut d.pix, d.stride, 4),
        (Image::Rgba64(d), Image::Rgba64(s)) => (&s.pix, &mut d.pix, d.stride, 8),
        (Image::Paletted(d), Image::Paletted(s)) => {
            if d.palette.len() < s.palette.len() {
                d.palette.clone_from(&s.palette);
            }
            (&s.pix.pix, &mut d.pix.pix, d.pix.stride, 1)
        }
        _ => return,
    };
    let b = src.bounds();
    let mut s = 0usize;
    for y in 0..b.dy().max(0) as usize {
        let d_base = (y * yf + yo) * stride + xo * bpp;
        for x in 0..b.dx().max(0) as usize {
            let d = d_base + x * xf * bpp;
            if let Some(chunk) = src_pix.get(s..s + bpp) {
                copy_at(dst_pix, d, chunk);
            }
            s += bpp;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adam7_pass_sizes_round_up_and_skip_empty_passes() {
        // A 1x1 image has pixels only in pass 0.
        let p = Params {
            cb: Cb::G8,
            depth: 8,
            width: 1,
            height: 1,
            interlace: IT_ADAM7,
            use_transparent: false,
            transparent: [0; 6],
            palette_backing: Vec::new(),
            palette_len: 0,
        };
        for pass in 1..7 {
            assert!(
                read_image_pass::<BytesReader>(&p, None, pass, false)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn a_short_header_is_unexpected_eof_and_a_wrong_one_is_not_png() {
        assert_eq!(decode(b"\x89PNG").err(), Some(Error::UnexpectedEof));
        assert_eq!(decode(b"").err(), Some(Error::UnexpectedEof));
        assert_eq!(
            decode(b"\x89PNX\r\n\x1a\n")
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("png: invalid format: not a PNG file")
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{b64, describe, fixture, palette_entry};
    use serde_json::{Value as Json, json};

    fn model_json(m: &ConfigModel) -> Json {
        match m {
            ConfigModel::Gray => json!("gray"),
            ConfigModel::Gray16 => json!("gray16"),
            ConfigModel::Rgba => json!("rgba"),
            ConfigModel::Rgba64 => json!("rgba64"),
            ConfigModel::Nrgba => json!("nrgba"),
            ConfigModel::Nrgba64 => json!("nrgba64"),
            ConfigModel::Palette(p) => {
                json!({ "palette": p.iter().map(palette_entry).collect::<Vec<_>>() })
            }
        }
    }

    /// Every file of the oracle's PNG corpus through `image.DecodeConfig` and `image.Decode`:
    /// the dimensions and colour model, and the decoded image's type, geometry and pixels — or
    /// Go's error text, byte for byte.
    #[test]
    fn decode_matches_go_on_every_corpus_file() {
        let cases = fixture("png")["decode"].as_array().unwrap();
        let (mut checked, mut skipped) = (0, 0);
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            if !data.starts_with(PNG_HEADER) {
                // image.Decode's format sniffing, not the PNG decoder: "image: unknown format".
                assert_eq!(c["image"]["err"], "image: unknown format", "{name}");
                skipped += 1;
                continue;
            }
            let config = match decode_config(&data) {
                Ok(cfg) => json!({
                    "w": cfg.width, "h": cfg.height, "format": "png", "model": model_json(&cfg.model)
                }),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(config, c["config"], "{name}: config");
            let image = match decode(&data) {
                Ok(img) => {
                    let mut d = describe(&img);
                    d["format"] = json!("png");
                    d
                }
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(image, c["image"], "{name}: image");
            checked += 1;
        }
        assert!(checked > 120, "{checked}");
        assert_eq!(skipped, 3, "files without the PNG signature");
    }
}
