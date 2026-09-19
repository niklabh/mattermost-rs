//! Port of `image/png/writer.go`: Go's PNG encoder.
//!
//! The bytes depend on three choices this port reproduces exactly: the colour type picked from
//! the image's Go type and opacity (`Encoder.Encode`), the per-row filter heuristic (`filter`,
//! which tries the five filters in Go's order and stops summing early), and the writer chain —
//! zlib into a 32 KiB `bufio.Writer` whose every flush becomes one IDAT chunk.

use crate::bufio::Writer as BufWriter;
use crate::hash::Crc32;
use crate::image::{Color, Image, Model};
use crate::sink::Sink;
use crate::zlib::{Writer as ZlibWriter, ZlibError};

const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n";

const CT_GRAYSCALE: u8 = 0;
const CT_TRUE_COLOR: u8 = 2;
const CT_PALETTED: u8 = 3;
const CT_TRUE_COLOR_ALPHA: u8 = 6;

const FT_NONE: usize = 0;
const FT_SUB: usize = 1;
const FT_UP: usize = 2;
const FT_AVERAGE: usize = 3;
const FT_PAETH: usize = 4;
const N_FILTER: usize = 5;

/// zlib levels (`compress/zlib`), for `levelToZlib`.
const ZLIB_NO_COMPRESSION: i32 = 0;

/// The `cb` values the encoder can choose (reader.go:31), bit depth folded into colour type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cb {
    G8,
    Tc8,
    P1,
    P2,
    P4,
    P8,
    Tca8,
    G16,
    Tc16,
    Tca16,
}

impl Cb {
    fn is_paletted(self) -> bool {
        matches!(self, Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8)
    }

    /// `(bit depth, colour type)` for IHDR (writer.go:126).
    fn ihdr(self) -> (u8, u8) {
        match self {
            Cb::G8 => (8, CT_GRAYSCALE),
            Cb::Tc8 => (8, CT_TRUE_COLOR),
            Cb::P8 => (8, CT_PALETTED),
            Cb::P4 => (4, CT_PALETTED),
            Cb::P2 => (2, CT_PALETTED),
            Cb::P1 => (1, CT_PALETTED),
            Cb::Tca8 => (8, CT_TRUE_COLOR_ALPHA),
            Cb::G16 => (16, CT_GRAYSCALE),
            Cb::Tc16 => (16, CT_TRUE_COLOR),
            Cb::Tca16 => (16, CT_TRUE_COLOR_ALPHA),
        }
    }

    /// `bitsPerPixel` (writer.go:327).
    fn bits_per_pixel(self) -> usize {
        match self {
            Cb::G8 | Cb::P8 => 8,
            Cb::Tc8 => 24,
            Cb::P4 => 4,
            Cb::P2 => 2,
            Cb::P1 => 1,
            Cb::Tca8 => 32,
            Cb::Tc16 => 48,
            Cb::Tca16 => 64,
            Cb::G16 => 16,
        }
    }
}

/// Port of `png.CompressionLevel` (writer.go:52).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompressionLevel {
    /// `png.DefaultCompression` → zlib level -1.
    #[default]
    Default,
    /// `png.NoCompression` → zlib level 0.
    NoCompression,
    /// `png.BestSpeed` → zlib level 1, whose deflater is not ported ([`PngEncodeError::Zlib`]).
    BestSpeed,
    /// `png.BestCompression` → zlib level 9. Mattermost's encoder (`imaging.NewEncoder`).
    BestCompression,
}

impl CompressionLevel {
    /// `levelToZlib` (writer.go:606).
    fn to_zlib(self) -> i32 {
        match self {
            CompressionLevel::Default => -1,
            CompressionLevel::NoCompression => 0,
            CompressionLevel::BestSpeed => 1,
            CompressionLevel::BestCompression => 9,
        }
    }
}

/// Errors from `png.Encoder.Encode`, with Go's text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PngEncodeError {
    /// `png.FormatError` — Go's text is `png: invalid format: ` plus the detail.
    #[error("png: invalid format: {0}")]
    Format(String),
    /// The deflater refused the level.
    #[error(transparent)]
    Zlib(#[from] ZlibError),
}

/// `abs8` (writer.go:94): a byte read as a signed int8, made absolute.
fn abs8(d: u8) -> i64 {
    if d < 128 {
        i64::from(d)
    } else {
        256 - i64::from(d)
    }
}

/// Port of `paeth` (paeth.go:23).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let pc = i32::from(c);
    let pa = i32::from(b) - pc;
    let pb = i32::from(a) - pc;
    let pc = (pa + pb).abs();
    let pa = pa.abs();
    let pb = pb.abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Port of `filter` (writer.go:224): try Up, Paeth, None, Sub, Average in that order, keeping the
/// smallest sum of absolute signed bytes; every filter after the first stops summing once it can
/// no longer win. Returns the chosen filter, whose row in `cr` holds the filtered bytes.
fn filter(cr: &mut [Vec<u8>; N_FILTER], pr: &[u8], bpp: usize) -> usize {
    let (c0, rest) = cr.split_at_mut(1);
    let cdat0 = &c0[0][1..];
    let (c1, rest) = rest.split_at_mut(1);
    let (c2, rest) = rest.split_at_mut(1);
    let (c3, c4) = rest.split_at_mut(1);
    let cdat1 = &mut c1[0][1..];
    let cdat2 = &mut c2[0][1..];
    let cdat3 = &mut c3[0][1..];
    let cdat4 = &mut c4[0][1..];
    let pdat = &pr[1..];
    let n = cdat0.len();

    // Up.
    let mut sum = 0;
    for i in 0..n {
        cdat2[i] = cdat0[i].wrapping_sub(pdat[i]);
        sum += abs8(cdat2[i]);
    }
    let mut best = sum;
    let mut filter = FT_UP;

    // Paeth.
    sum = 0;
    for i in 0..bpp {
        cdat4[i] = cdat0[i].wrapping_sub(pdat[i]);
        sum += abs8(cdat4[i]);
    }
    for i in bpp..n {
        cdat4[i] = cdat0[i].wrapping_sub(paeth(cdat0[i - bpp], pdat[i], pdat[i - bpp]));
        sum += abs8(cdat4[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_PAETH;
    }

    // None.
    sum = 0;
    for &c in cdat0 {
        sum += abs8(c);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_NONE;
    }

    // Sub.
    sum = 0;
    for i in 0..bpp {
        cdat1[i] = cdat0[i];
        sum += abs8(cdat1[i]);
    }
    for i in bpp..n {
        cdat1[i] = cdat0[i].wrapping_sub(cdat0[i - bpp]);
        sum += abs8(cdat1[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = FT_SUB;
    }

    // Average.
    sum = 0;
    for i in 0..bpp {
        cdat3[i] = cdat0[i].wrapping_sub(pdat[i] / 2);
        sum += abs8(cdat3[i]);
    }
    for i in bpp..n {
        cdat3[i] =
            cdat0[i].wrapping_sub(((u32::from(cdat0[i - bpp]) + u32::from(pdat[i])) / 2) as u8);
        sum += abs8(cdat3[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        filter = FT_AVERAGE;
    }
    filter
}

/// Port of `encoder.writeChunk` (writer.go:102): length, type, data, CRC — three `Write`s.
fn write_chunk(w: &mut impl Sink, b: &[u8], name: &[u8; 4]) {
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&(b.len() as u32).to_be_bytes());
    header[4..].copy_from_slice(name);
    let mut crc = Crc32::new();
    crc.update(name);
    crc.update(b);
    w.write(&header);
    w.write(b);
    w.write(&crc.sum().to_be_bytes());
}

/// Port of `encoder.Write` (writer.go:212): every write that reaches it is one IDAT chunk.
struct IdatWriter<'a, S: Sink> {
    out: &'a mut S,
}

impl<S: Sink> Sink for IdatWriter<'_, S> {
    fn write(&mut self, p: &[u8]) {
        write_chunk(self.out, p, b"IDAT");
    }
}

/// Port of `opaque` (writer.go:79).
fn opaque(m: &Image) -> bool {
    if let Some(o) = m.opaque_method() {
        return o;
    }
    let b = m.bounds();
    for y in b.min_y..b.max_y {
        for x in b.min_x..b.max_x {
            if m.at(x, y).map(|c| c.rgba().3) != Some(0xffff) {
                return false;
            }
        }
    }
    true
}

/// `m.At(x, y)`, with Go's nil colour (an empty palette) read as transparent black.
fn at(m: &Image, x: i64, y: i64) -> Color {
    m.at(x, y).unwrap_or(Color::Rgba([0; 4]))
}

/// Port of `encoder.writeImage` (writer.go:300): convert each row to bytes, filter it, and feed it
/// to zlib as one `Write`.
fn write_image<S: Sink>(w: S, m: &Image, cb: Cb, level: i32) -> Result<S, ZlibError> {
    let mut zw = ZlibWriter::new(w, level)?;
    let bits_per_pixel = cb.bits_per_pixel();
    let b = m.bounds();
    let dx = b.dx() as usize;
    let sz = 1 + (bits_per_pixel * dx).div_ceil(8);
    let mut cr: [Vec<u8>; N_FILTER] = std::array::from_fn(|i| {
        let mut v = vec![0u8; sz];
        v[0] = i as u8;
        v
    });
    let mut pr = vec![0u8; sz];

    for y in b.min_y..b.max_y {
        let row = &mut cr[0];
        let mut i = 1usize;
        match cb {
            Cb::G8 => {
                if let Image::Gray(g) = m {
                    let off = (y - b.min_y) as usize * g.stride;
                    row[1..].copy_from_slice(&g.pix[off..off + dx]);
                } else {
                    for x in b.min_x..b.max_x {
                        row[i] = at(m, x, y).to_gray();
                        i += 1;
                    }
                }
            }
            Cb::Tc8 => {
                // Opacity was checked when cb was chosen.
                match m {
                    Image::Rgba(p) | Image::Nrgba(p) if p.stride != 0 => {
                        let j0 = (y - b.min_y) as usize * p.stride;
                        for px in p.pix[j0..j0 + dx * 4].chunks_exact(4) {
                            row[i..i + 3].copy_from_slice(&px[..3]);
                            i += 3;
                        }
                    }
                    _ => {
                        for x in b.min_x..b.max_x {
                            let (r, g, bb, _) = at(m, x, y).rgba();
                            row[i] = (r >> 8) as u8;
                            row[i + 1] = (g >> 8) as u8;
                            row[i + 2] = (bb >> 8) as u8;
                            i += 3;
                        }
                    }
                }
            }
            Cb::P8 => {
                if let Image::Paletted(p) = m {
                    let off = (y - b.min_y) as usize * p.pix.stride;
                    row[1..].copy_from_slice(&p.pix.pix[off..off + dx]);
                }
            }
            Cb::P4 | Cb::P2 | Cb::P1 => {
                if let Image::Paletted(p) = m {
                    let mut a: u8 = 0;
                    let mut c = 0;
                    let pixels_per_byte = 8 / bits_per_pixel;
                    for x in b.min_x..b.max_x {
                        let idx = p.pix.pix[p.pix.offset(x, y, 1)];
                        a = (a << bits_per_pixel) | idx;
                        c += 1;
                        if c == pixels_per_byte {
                            row[i] = a;
                            i += 1;
                            a = 0;
                            c = 0;
                        }
                    }
                    if c != 0 {
                        while c != pixels_per_byte {
                            a <<= bits_per_pixel;
                            c += 1;
                        }
                        row[i] = a;
                    }
                }
            }
            Cb::Tca8 => match m {
                Image::Nrgba(p) => {
                    let off = (y - b.min_y) as usize * p.stride;
                    row[1..].copy_from_slice(&p.pix[off..off + dx * 4]);
                }
                Image::Rgba(p) => {
                    let start = p.offset(b.min_x, y, 4);
                    let src = &p.pix[start..start + dx * 4];
                    for (d, s) in row[1..].chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                        if s[3] == 0 {
                            d.copy_from_slice(&[0, 0, 0, 0]);
                        } else if s[3] == 0xff {
                            d.copy_from_slice(s);
                        } else {
                            const M: u32 = 0x101 * 0xffff;
                            let a = u32::from(s[3]) * 0x101;
                            d[0] = ((u32::from(s[0]) * M / a) >> 8) as u8;
                            d[1] = ((u32::from(s[1]) * M / a) >> 8) as u8;
                            d[2] = ((u32::from(s[2]) * M / a) >> 8) as u8;
                            d[3] = s[3];
                        }
                    }
                }
                _ => {
                    for x in b.min_x..b.max_x {
                        let c = at(m, x, y).to_nrgba();
                        row[i..i + 4].copy_from_slice(&c);
                        i += 4;
                    }
                }
            },
            Cb::G16 => {
                for x in b.min_x..b.max_x {
                    let c = at(m, x, y).to_gray16();
                    row[i..i + 2].copy_from_slice(&c.to_be_bytes());
                    i += 2;
                }
            }
            Cb::Tc16 => {
                for x in b.min_x..b.max_x {
                    let (r, g, bb, _) = at(m, x, y).rgba();
                    row[i..i + 2].copy_from_slice(&(r as u16).to_be_bytes());
                    row[i + 2..i + 4].copy_from_slice(&(g as u16).to_be_bytes());
                    row[i + 4..i + 6].copy_from_slice(&(bb as u16).to_be_bytes());
                    i += 6;
                }
            }
            Cb::Tca16 => {
                for x in b.min_x..b.max_x {
                    let c = at(m, x, y).to_nrgba64();
                    for (k, v) in c.iter().enumerate() {
                        row[i + 2 * k..i + 2 * k + 2].copy_from_slice(&v.to_be_bytes());
                    }
                    i += 8;
                }
            }
        }

        let mut f = FT_NONE;
        if level != ZLIB_NO_COMPRESSION && !cb.is_paletted() {
            f = filter(&mut cr, &pr, bits_per_pixel / 8);
        }
        zw.write(&cr[f]);
        std::mem::swap(&mut pr, &mut cr[0]);
    }
    zw.close();
    Ok(zw.into_inner())
}

/// Port of `png.Encoder.Encode` (writer.go:627) with `Encoder{CompressionLevel: level}`.
///
/// Errors are Go's: an image with a non-positive or ≥2³² dimension, and a palette outside
/// 1..=256 entries. On an error nothing useful has been written, as in Go, where the caller
/// discards the buffer.
pub fn encode<S: Sink>(
    w: &mut S,
    m: &Image,
    level: CompressionLevel,
) -> Result<(), PngEncodeError> {
    let bounds = m.bounds();
    let (mw, mh) = (bounds.dx(), bounds.dy());
    if mw <= 0 || mh <= 0 || mw >= 1 << 32 || mh >= 1 << 32 {
        return Err(PngEncodeError::Format(format!(
            "invalid image size: {mw}x{mh}"
        )));
    }

    let pal = match m {
        Image::Paletted(p) => Some(&p.palette),
        _ => None,
    };
    let cb = if let Some(pal) = pal {
        match pal.len() {
            0..=2 => Cb::P1,
            3..=4 => Cb::P2,
            5..=16 => Cb::P4,
            _ => Cb::P8,
        }
    } else {
        match m.model() {
            Model::Gray => Cb::G8,
            Model::Gray16 => Cb::G16,
            Model::Rgba | Model::Nrgba => {
                if opaque(m) {
                    Cb::Tc8
                } else {
                    Cb::Tca8
                }
            }
            _ => {
                if opaque(m) {
                    Cb::Tc16
                } else {
                    Cb::Tca16
                }
            }
        }
    };

    // Go's zlib writer (and so the level check) only comes into play in writeIDATs, after the
    // header, IHDR and PLTE are out; a refused level surfaces as an error there too.
    w.write(PNG_HEADER);
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&(mw as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(mh as u32).to_be_bytes());
    let (depth, ct) = cb.ihdr();
    ihdr[8] = depth;
    ihdr[9] = ct;
    write_chunk(w, &ihdr, b"IHDR");

    if let Some(p) = pal {
        // writePLTEAndTRNS (writer.go:162).
        if p.is_empty() || p.len() > 256 {
            return Err(PngEncodeError::Format(format!(
                "bad palette length: {}",
                p.len()
            )));
        }
        let mut plte = Vec::with_capacity(3 * p.len());
        let mut trns = Vec::with_capacity(p.len());
        let mut last = None;
        for (i, c) in p.iter().enumerate() {
            let c1 = c.to_nrgba();
            plte.extend_from_slice(&c1[..3]);
            if c1[3] != 0xff {
                last = Some(i);
            }
            trns.push(c1[3]);
        }
        write_chunk(w, &plte, b"PLTE");
        if let Some(last) = last {
            write_chunk(w, &trns[..=last], b"tRNS");
        }
    }

    // writeIDATs (writer.go:590): zlib → bufio(32 KiB) → one IDAT per flush.
    let idat = IdatWriter { out: &mut *w };
    let bw = BufWriter::with_capacity(1 << 15, idat);
    let mut bw = write_image(bw, m, cb, level.to_zlib())?;
    bw.flush();
    drop(bw);

    write_chunk(w, &[], b"IEND");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Paletted, Pixels, Rect};

    #[test]
    fn paeth_matches_the_spec_predictor() {
        for a in [0u8, 7, 128, 255] {
            for b in [0u8, 9, 200, 255] {
                for c in [0u8, 3, 130, 255] {
                    let p = i32::from(a) + i32::from(b) - i32::from(c);
                    let (pa, pb, pc) = (
                        (p - i32::from(a)).abs(),
                        (p - i32::from(b)).abs(),
                        (p - i32::from(c)).abs(),
                    );
                    let want = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    assert_eq!(paeth(a, b, c), want);
                }
            }
        }
    }

    #[test]
    fn zero_sized_and_empty_palette_images_are_go_errors() {
        let m = Image::Nrgba(Pixels::new(Rect::new(0, 0, 0, 3), 4));
        assert_eq!(
            encode(&mut Vec::new(), &m, CompressionLevel::BestCompression)
                .unwrap_err()
                .to_string(),
            "png: invalid format: invalid image size: 0x3"
        );
        let m = Image::Paletted(Paletted {
            pix: Pixels::new(Rect::new(0, 0, 2, 2), 1),
            palette: vec![],
        });
        assert_eq!(
            encode(&mut Vec::new(), &m, CompressionLevel::BestCompression)
                .unwrap_err()
                .to_string(),
            "png: invalid format: bad palette length: 0"
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{assert_encoded, build, fixture};

    /// Every `encode` case of behaviour_imaging_png.json: every Go image type, sizes from 1×1 to
    /// 1920×1080 (the latter crossing the 32 KiB IDAT boundary many times), at Mattermost's
    /// BestCompression and at DefaultCompression.
    #[test]
    fn every_png_encode_case_matches_go_byte_for_byte() {
        let cases = fixture("png")["encode"].as_array().unwrap();
        let mut n = 0;
        for c in cases {
            let m = build(&c["spec"]);
            let level = match c["level"].as_str().unwrap() {
                "best" => CompressionLevel::BestCompression,
                "default" => CompressionLevel::Default,
                other => panic!("{other}"),
            };
            let mut out = Vec::new();
            let got = encode(&mut out, &m, level);
            match c["err"].as_str() {
                Some(e) => assert_eq!(got.unwrap_err().to_string(), e, "{}", c["spec"]),
                None => {
                    got.unwrap();
                    assert_encoded(&format!("{} {}", c["spec"], c["level"]), &out, &c["output"]);
                }
            }
            n += 1;
        }
        assert!(n > 200, "{n}");
    }
}
