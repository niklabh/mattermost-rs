//! Port of `golang.org/x/image/webp` (webp/decode.go, riff/riff.go, vp8/, vp8l/): the WEBP
//! container and both of its bitstreams.
//!
//! `webp.Decode` walks a RIFF stream whose form type is `WEBP` and stops at the first `VP8 `
//! (lossy) or `VP8L` (lossless) chunk. A `VP8X` chunk before it declares the canvas size and
//! whether an `ALPH` chunk follows; everything else — `ICCP`, `EXIF`, `XMP `, a `LIST` — is skipped.
//!
//! # Result types
//!
//! Lossy decodes to an `*image.YCbCr` (4:2:0), lossless to an `*image.NRGBA`, and a lossy frame
//! *with* an `ALPH` chunk to an `*image.NYCbCrA`. [`crate::image::Image`] has no `NYCbCrA` variant,
//! so [`decode`] returns [`Error::NycbcraUnsupported`] for that last case and nothing else; the
//! planes themselves are decoded correctly and are reachable through [`decode_frame`], which the
//! parity tests assert against Go's hashes.
//!
//! # Allocation
//!
//! A chunk header is attacker-controlled and may claim far more data than the file holds. Go sizes
//! `make` from the claim and then fails the read; every such allocation here grows in steps
//! instead, so a lie costs a megabyte rather than the lie. The bytes and the error are the same.

pub mod riff;
pub mod vp8;
pub mod vp8l;

use crate::goread::{self, BufReader, BytesReader, Read as GoRead};
use crate::image::{Image, YCbCr};

/// Every error `webp.Decode` and the packages under it can produce. `Display` is Go's
/// `err.Error()`, byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `errInvalidFormat` (decode.go:19), and the identically worded `errors.New` in `readAlpha`.
    #[error("webp: invalid format")]
    InvalidFormat,
    /// An error from the RIFF container.
    #[error(transparent)]
    Riff(#[from] riff::Error),
    /// An error from the lossy bitstream.
    #[error(transparent)]
    Vp8(#[from] vp8::Error),
    /// An error from the lossless bitstream.
    #[error(transparent)]
    Vp8l(#[from] vp8l::Error),
    /// An error from the underlying reader.
    #[error(transparent)]
    Io(#[from] goread::Error),
    /// Not a Go error. Go answers a lossy frame with an `ALPH` chunk with an `*image.NYCbCrA`,
    /// which [`crate::image::Image`] cannot hold; [`decode_frame`] returns the planes instead.
    #[error("webp: lossy image with an alpha chunk decodes to *image.NYCbCrA, which is not ported")]
    NycbcraUnsupported,
}

/// The colour model `webp.DecodeConfig` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigModel {
    /// `color.YCbCrModel`: a lossy frame, or a `VP8X` canvas without the alpha bit.
    YCbCr,
    /// `color.NRGBAModel`: a lossless frame.
    Nrgba,
    /// `color.NYCbCrAModel`: a `VP8X` canvas with the alpha bit set.
    Nycbcra,
}

/// `image.Config` for a WEBP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: ConfigModel,
}

/// The alpha-carrying lossy frame Go returns as an `*image.NYCbCrA`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nycbcra {
    pub ycbcr: YCbCr,
    pub a: Vec<u8>,
    pub a_stride: usize,
}

/// What a full decode produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// A lossy `*image.YCbCr` or a lossless `*image.NRGBA`.
    Image(Image),
    /// A lossy frame with an alpha chunk.
    Nycbcra(Nycbcra),
}

const FCC_ALPH: &[u8; 4] = b"ALPH";
const FCC_VP8: &[u8; 4] = b"VP8 ";
const FCC_VP8L: &[u8; 4] = b"VP8L";
const FCC_VP8X: &[u8; 4] = b"VP8X";
const FCC_WEBP: &[u8; 4] = b"WEBP";

/// Port of `webp.Decode`.
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    match decode_frame(data)? {
        Frame::Image(m) => Ok(m),
        Frame::Nycbcra(_) => Err(Error::NycbcraUnsupported),
    }
}

/// `webp.Decode` without the `*image.NYCbCrA` restriction [`decode`] imposes.
pub fn decode_frame(data: &[u8]) -> Result<Frame, Error> {
    match decode_raw(data, false)? {
        Out::Frame(f) => Ok(f),
        // `decode_raw` only takes a config exit when `config_only` is set.
        Out::Config(_) => Err(Error::InvalidFormat),
    }
}

/// Port of `webp.DecodeConfig`.
pub fn decode_config(data: &[u8]) -> Result<Config, Error> {
    match decode_raw(data, true)? {
        Out::Config(c) => Ok(c),
        // Every `config_only` path returns a config; none reaches a frame.
        Out::Frame(_) => Err(Error::InvalidFormat),
    }
}

enum Out {
    Config(Config),
    Frame(Frame),
}

/// Port of `webp.decode` (decode.go:30): the chunk loop both entry points share.
fn decode_raw(data: &[u8], config_only: bool) -> Result<Out, Error> {
    let (form_type, mut rr) = riff::new_reader(data)?;
    if &form_type != FCC_WEBP {
        return Err(Error::InvalidFormat);
    }

    let mut alpha: Option<Vec<u8>> = None;
    let mut alpha_stride = 0usize;
    let mut want_alpha = false;
    let mut seen_vp8x = false;
    let mut width_minus_one = 0u32;
    let mut height_minus_one = 0u32;
    let mut buf = [0u8; 10];
    loop {
        let (chunk_id, chunk_len) = match rr.next() {
            Ok(v) => v,
            Err(e) if e.is_eof() => return Err(Error::InvalidFormat),
            Err(e) => return Err(Error::Riff(e)),
        };

        match &chunk_id {
            FCC_ALPH => {
                if !want_alpha {
                    return Err(Error::InvalidFormat);
                }
                want_alpha = false;
                // Read the Pre-processing | Filter | Compression byte.
                if let Err(e) = goread::read_full_err(&mut rr.chunk(), &mut buf[..1]) {
                    return Err(if e == goread::Error::Eof {
                        Error::InvalidFormat
                    } else {
                        Error::Io(e)
                    });
                }
                let (mut a, stride) =
                    read_alpha(&mut rr, width_minus_one, height_minus_one, buf[0] & 0x03)?;
                unfilter_alpha(&mut a, stride, (buf[0] >> 2) & 0x03);
                alpha = Some(a);
                alpha_stride = stride;
            }

            FCC_VP8 => {
                if want_alpha || (chunk_len as i32) < 0 {
                    return Err(Error::InvalidFormat);
                }
                let mut cd = rr.chunk();
                let mut d = vp8::Decoder::new(&mut cd, chunk_len as usize);
                let fh = d.decode_frame_header()?;
                if seen_vp8x
                    && (fh.width != width_minus_one as i32 + 1
                        || fh.height != height_minus_one as i32 + 1)
                {
                    return Err(Error::InvalidFormat);
                }
                if config_only {
                    return Ok(Out::Config(Config {
                        width: i64::from(fh.width),
                        height: i64::from(fh.height),
                        model: ConfigModel::YCbCr,
                    }));
                }
                let m = d.decode_frame()?;
                return Ok(Out::Frame(match alpha {
                    Some(a) => Frame::Nycbcra(Nycbcra {
                        ycbcr: m,
                        a,
                        a_stride: alpha_stride,
                    }),
                    None => Frame::Image(Image::YCbCr(m)),
                }));
            }

            FCC_VP8L => {
                if alpha.is_some() {
                    return Err(Error::InvalidFormat);
                }
                if config_only {
                    let mut cd = rr.chunk();
                    let mut br = BufReader::new(&mut cd);
                    let c = vp8l::decode_config(&mut br)?;
                    return Ok(Out::Config(Config {
                        width: i64::from(c.width),
                        height: i64::from(c.height),
                        model: ConfigModel::Nrgba,
                    }));
                }
                if seen_vp8x {
                    // Verify the VP8L chunk dimensions match before decoding it, to catch a
                    // malicious image following a small VP8X header with a huge VP8L chunk.
                    const VP8L_HEADER_SIZE: usize = 5;
                    let header = rr.peek_chunk(VP8L_HEADER_SIZE);
                    if header.len() < VP8L_HEADER_SIZE {
                        // Go's bufio.Peek reports io.EOF for a short chunk, which webp remaps.
                        return Err(Error::InvalidFormat);
                    }
                    let mut hr = BytesReader::new(header);
                    let c = vp8l::decode_config(&mut hr)?;
                    if c.width != width_minus_one as i32 + 1
                        || c.height != height_minus_one as i32 + 1
                    {
                        return Err(Error::InvalidFormat);
                    }
                }
                let mut cd = rr.chunk();
                let mut br = BufReader::new(&mut cd);
                let m = vp8l::decode(&mut br)?;
                return Ok(Out::Frame(Frame::Image(m)));
            }

            FCC_VP8X => {
                if seen_vp8x {
                    return Err(Error::InvalidFormat);
                }
                seen_vp8x = true;
                if chunk_len != 10 {
                    return Err(Error::InvalidFormat);
                }
                goread::read_full_err(&mut rr.chunk(), &mut buf[..10])?;
                const ALPHA_BIT: u8 = 1 << 4;
                want_alpha = (buf[0] & ALPHA_BIT) != 0;
                width_minus_one =
                    u32::from(buf[4]) | u32::from(buf[5]) << 8 | u32::from(buf[6]) << 16;
                height_minus_one =
                    u32::from(buf[7]) | u32::from(buf[8]) << 8 | u32::from(buf[9]) << 16;
                let w = u64::from(width_minus_one) + 1;
                let h = u64::from(height_minus_one) + 1;
                if w * h > (1 << 31) - 1 {
                    // The product of the canvas dimensions must be at most 2^32 - 1, and it also
                    // has to fit in an int, so Go limits it to MaxInt32.
                    return Err(Error::InvalidFormat);
                }
                if config_only {
                    return Ok(Out::Config(Config {
                        width: i64::from(width_minus_one) + 1,
                        height: i64::from(height_minus_one) + 1,
                        model: if want_alpha {
                            ConfigModel::Nycbcra
                        } else {
                            ConfigModel::YCbCr
                        },
                    }));
                }
            }

            // Every other chunk is skipped; the next `Next` drains it.
            _ => {}
        }
    }
}

/// Port of `readAlpha` (decode.go:183).
fn read_alpha(
    rr: &mut riff::Reader<'_>,
    width_minus_one: u32,
    height_minus_one: u32,
    compression: u8,
) -> Result<(Vec<u8>, usize), Error> {
    match compression {
        0 => {
            let w = width_minus_one as usize + 1;
            let h = height_minus_one as usize + 1;
            let alpha = read_full_alloc(&mut rr.chunk(), w * h)?;
            Ok((alpha, w))
        }

        1 => {
            // Read the VP8L-compressed alpha values. First, synthesize a 5-byte VP8L header: a
            // 1-byte magic number, a 14-bit widthMinusOne, a 14-bit heightMinusOne, a 1-bit
            // (ignored, zero) alphaIsUsed and a 3-bit (zero) version.
            if width_minus_one > 0x3fff || height_minus_one > 0x3fff {
                return Err(Error::InvalidFormat);
            }
            let header = [
                0x2f, // VP8L magic number.
                width_minus_one as u8,
                (width_minus_one >> 8) as u8 | (height_minus_one << 6) as u8,
                (height_minus_one >> 2) as u8,
                (height_minus_one >> 10) as u8,
            ];
            let mut head = BytesReader::new(&header);
            let mut cd = rr.chunk();
            let mut multi = MultiReader::new(&mut head, &mut cd);
            let mut br = BufReader::new(&mut multi);
            let alpha_image = vp8l::decode(&mut br)?;
            // The green values of the inner NRGBA image are the alpha values of the outer
            // NYCbCrA image.
            let Image::Nrgba(p) = alpha_image else {
                // vp8l::decode returns nothing else.
                return Err(Error::InvalidFormat);
            };
            let mut alpha = vec![0u8; p.pix.len() / 4];
            for (i, a) in alpha.iter_mut().enumerate() {
                *a = p.pix[4 * i + 1];
            }
            Ok((alpha, width_minus_one as usize + 1))
        }

        _ => Err(Error::InvalidFormat),
    }
}

/// Port of `unfilterAlpha` (decode.go:229).
///
/// `alpha` is always a whole number of rows — `readAlpha` builds it as `w*h` bytes with a stride
/// of `w` — and the loops below are written over that invariant rather than over `len(alpha)` as
/// Go's are, so a ragged buffer would drop its last partial row instead of indexing past the end.
fn unfilter_alpha(alpha: &mut [u8], alpha_stride: usize, filter: u8) {
    if alpha.is_empty() || alpha_stride == 0 {
        return;
    }
    let rows = alpha.len() / alpha_stride;
    // The first row is equivalent to the horizontal filter under every mode that uses one.
    let horizontal_first_row = |alpha: &mut [u8]| {
        for i in 1..alpha_stride {
            alpha[i] = alpha[i].wrapping_add(alpha[i - 1]);
        }
    };
    match filter {
        // Horizontal filter.
        1 => {
            horizontal_first_row(alpha);
            for row in 1..rows {
                let i = row * alpha_stride;
                // The first column is equivalent to the vertical filter.
                alpha[i] = alpha[i].wrapping_add(alpha[i - alpha_stride]);
                for j in 1..alpha_stride {
                    alpha[i + j] = alpha[i + j].wrapping_add(alpha[i + j - 1]);
                }
            }
        }

        // Vertical filter.
        2 => {
            horizontal_first_row(alpha);
            for i in alpha_stride..alpha.len() {
                alpha[i] = alpha[i].wrapping_add(alpha[i - alpha_stride]);
            }
        }

        // Gradient filter.
        3 => {
            horizontal_first_row(alpha);
            for row in 1..rows {
                let i = row * alpha_stride;
                // The first column is equivalent to the vertical filter.
                alpha[i] = alpha[i].wrapping_add(alpha[i - alpha_stride]);

                // The interior is predicted on the three top/left pixels.
                for j in 1..alpha_stride {
                    let c = i32::from(alpha[i + j - alpha_stride - 1]);
                    let b = i32::from(alpha[i + j - alpha_stride]);
                    let a = i32::from(alpha[i + j - 1]);
                    let x = (a + b - c).clamp(0, 255);
                    alpha[i + j] = alpha[i + j].wrapping_add(x as u8);
                }
            }
        }

        _ => {}
    }
}

/// Port of `io.MultiReader` over two readers.
struct MultiReader<'a> {
    readers: [Option<&'a mut dyn GoRead>; 2],
}

impl<'a> MultiReader<'a> {
    fn new(a: &'a mut dyn GoRead, b: &'a mut dyn GoRead) -> Self {
        MultiReader {
            readers: [Some(a), Some(b)],
        }
    }
}

impl GoRead for MultiReader<'_> {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<goread::Error>) {
        for i in 0..self.readers.len() {
            if self.readers[i].is_none() {
                continue;
            }
            let (n, err) = match &mut self.readers[i] {
                Some(r) => r.read(p),
                None => continue,
            };
            let at_eof = err == Some(goread::Error::Eof);
            if at_eof {
                self.readers[i] = None;
            }
            if n > 0 || !at_eof {
                // Go hides the inner io.EOF while a later reader is still to come.
                let more = self.readers.iter().skip(i + 1).any(Option::is_some);
                return (n, if at_eof && more { None } else { err });
            }
        }
        (0, Some(goread::Error::Eof))
    }
}

/// `io.ReadFull` into a buffer of `n` bytes that is grown in steps rather than allocated from a
/// chunk header's unverified claim. The bytes read and the error are `io.ReadFull`'s.
fn read_full_alloc(r: &mut dyn GoRead, n: usize) -> Result<Vec<u8>, goread::Error> {
    const STEP: usize = 1 << 20;
    let mut v: Vec<u8> = Vec::new();
    let mut total = 0usize;
    while v.len() < n {
        let base = v.len();
        v.resize(base + (n - base).min(STEP), 0);
        let (got, err) = goread::read_full(r, &mut v[base..]);
        total += got;
        if let Some(e) = err {
            return Err(if e == goread::Error::Eof && total > 0 {
                goread::Error::UnexpectedEof
            } else {
                e
            });
        }
    }
    Ok(v)
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{b64, describe, fixture, sha};
    use serde_json::{Value as Json, json};

    fn model_json(m: ConfigModel) -> Json {
        match m {
            ConfigModel::YCbCr => json!("ycbcr"),
            ConfigModel::Nrgba => json!("nrgba"),
            ConfigModel::Nycbcra => json!("nycbcra"),
        }
    }

    /// Mirror of the oracle's `webpImageOf`, including the `*image.NYCbCrA` branch it spells out
    /// because `describe` has no case for one.
    fn describe_frame(f: &Frame) -> Json {
        match f {
            Frame::Image(m) => {
                let mut d = describe(m);
                d["format"] = json!("webp");
                d
            }
            Frame::Nycbcra(n) => {
                let r = n.ycbcr.rect;
                json!({
                    "type": "nycbcra",
                    "format": "webp",
                    "rect": [r.min_x, r.min_y, r.max_x, r.max_y],
                    "ratio": n.ycbcr.ratio.go_name(),
                    "y_stride": n.ycbcr.y_stride,
                    "c_stride": n.ycbcr.c_stride,
                    "a_stride": n.a_stride,
                    "y_sha256": sha(&n.ycbcr.y),
                    "cb_sha256": sha(&n.ycbcr.cb),
                    "cr_sha256": sha(&n.ycbcr.cr),
                    "a_sha256": sha(&n.a),
                    "y_len": n.ycbcr.y.len(),
                    "c_len": n.ycbcr.cb.len(),
                    "a_len": n.a.len(),
                })
            }
        }
    }

    /// Every case of the oracle's WebP corpus through `webp.DecodeConfig` and `webp.Decode`: the
    /// dimensions and colour model, and the decoded image's Go type, geometry, strides, plane
    /// lengths and plane hashes — or Go's error text, byte for byte.
    #[test]
    fn decode_matches_go_on_every_corpus_case() {
        let cases = fixture("webp")["decode"].as_array().unwrap();
        let mut checked = 0;
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let config = match decode_config(&data) {
                Ok(cfg) => json!({
                    "w": cfg.width, "h": cfg.height, "format": "webp",
                    "model": model_json(cfg.model),
                }),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(config, c["config"], "{name}: config");
            let image = match decode_frame(&data) {
                Ok(f) => describe_frame(&f),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(image, c["image"], "{name}: image");
            checked += 1;
        }
        assert!(checked > 330, "{checked}");
    }

    /// [`decode`] is [`decode_frame`] plus one refusal: a lossy frame with an alpha chunk, which
    /// Go answers with an `*image.NYCbCrA`. Nothing else in the corpus takes that branch, so this
    /// is exactly what a caller still has to forward.
    #[test]
    fn decode_refuses_nycbcra_and_nothing_else() {
        let cases = fixture("webp")["decode"].as_array().unwrap();
        let mut refused = 0;
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let is_nycbcra = c["image"]["type"] == json!("nycbcra");
            match decode(&data) {
                Err(Error::NycbcraUnsupported) => {
                    assert!(
                        is_nycbcra,
                        "{name}: refused a case Go did not answer with NYCbCrA"
                    );
                    refused += 1;
                }
                other => {
                    assert!(!is_nycbcra, "{name}: should have been refused");
                    let a = other.map(Frame::Image);
                    let b = decode_frame(&data);
                    assert_eq!(a.is_ok(), b.is_ok(), "{name}");
                    if let (Err(x), Err(y)) = (&a, &b) {
                        assert_eq!(x.to_string(), y.to_string(), "{name}");
                    }
                }
            }
        }
        // Six real and crafted files carry a lossy frame under an ALPH chunk.
        assert!(refused >= 6, "{refused}");
    }

    /// The registry contract: `image.DecodeConfig` reaches this decoder only for input matching
    /// Go's registered magic, `RIFF????WEBPVP8`, and answers `image: unknown format` otherwise.
    /// Whoever wires `format.rs` needs that prefix and not merely `RIFF`.
    #[test]
    fn the_sniff_prefix_is_riff_then_webp_then_vp8() {
        let cases = fixture("webp")["decode"].as_array().unwrap();
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let matches_magic = data.len() >= 15
                && &data[0..4] == b"RIFF"
                && &data[8..12] == b"WEBP"
                && &data[12..15] == b"VP8";
            let want = if !matches_magic {
                "image: unknown format".to_owned()
            } else {
                match decode_config(&data) {
                    Ok(_) => "webp".to_owned(),
                    Err(e) => e.to_string(),
                }
            };
            assert_eq!(c["sniff"], json!(want), "{name}");
        }
    }
}
