//! Port of `golang.org/x/image/bmp`'s decoder (bmp/reader.go, v0.44.0): `bmp.Decode` and
//! `bmp.DecodeConfig` as `image.Decode`/`image.DecodeConfig` call them. The encoder is not ported —
//! Mattermost registers this decoder in `channels/app/imaging/decode.go` and never writes a BMP.
//!
//! # The header is almost all of it
//!
//! There are only three pixel loops (a palette index stream at 1, 2, 4 or 8 bits, a 24-bit BGR
//! stream and a 32-bit BGRA one) and no compression: every other branch is `decodeConfig`
//! refusing a header. What it accepts is narrow — a BITMAPINFOHEADER, BITMAPV4HEADER or
//! BITMAPV5HEADER, one colour plane, no compression, and a pixel offset that is *exactly* where
//! the reader already is, because it never seeks. The one softening is that `BI_BITFIELDS` with
//! precisely the masks a compression of 0 implies is taken as a compression of 0, and only in a
//! header long enough to carry those masks.
//!
//! # The variant is part of the answer
//!
//! 24 bits per pixel decodes to `*image.RGBA`, 32 to `*image.NRGBA` and the rest to
//! `*image.Paletted` — while `DecodeConfig` reports `color.RGBAModel` for *both* 24 and 32, so the
//! model it reports does not name the type `Decode` returns. The resampler downstream dispatches
//! on the concrete type and indexes by stride, so [`ConfigModel::Rgba`] covering both is Go's
//! behaviour, not a simplification.
//!
//! # Allocation
//!
//! Like Go, the whole image is allocated from the header's dimensions, guarded only by
//! `safemath.Mul3(width, height, 4)` — which conservatively assumes four bytes per pixel. A
//! 65535×65535 header therefore still asks for 17 GiB here exactly as it does in Go; a caller
//! facing untrusted input runs `decode_config` against its resolution limit first, as Mattermost's
//! `imaging.Decoder` does.

use crate::goread::{BufReader, BytesReader, Error as ReadError, Read, read_full};
use crate::image::{Color, Image, Paletted, Pixels, Rect};

/// `fileHeaderLen` (reader.go:161): "BM", the file size, two reserved words and the pixel offset.
const FILE_HEADER_LEN: u32 = 14;
/// `infoHeaderLen`: BITMAPINFOHEADER.
const INFO_HEADER_LEN: u32 = 40;
/// `v4InfoHeaderLen`: BITMAPV4HEADER.
const V4_INFO_HEADER_LEN: u32 = 108;
/// `v5InfoHeaderLen`: BITMAPV5HEADER.
const V5_INFO_HEADER_LEN: u32 = 124;

/// Every error the decoder can produce, rendered with Go's text. `image.Decode` returns it
/// unchanged, so this is what a client sees.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Whatever the reader returned — `io.EOF` or `io.ErrUnexpectedEOF` in practice. Go hands the
    /// reader's error back untouched, so its text travels untouched too.
    #[error("{0}")]
    Read(#[from] ReadError),
    /// `errors.New("bmp: invalid format")` (reader.go:174): the file does not start "BM".
    #[error("bmp: invalid format")]
    InvalidFormat,
    /// `ErrUnsupported` (reader.go:21): a valid but unsupported feature.
    #[error("bmp: unsupported BMP image")]
    Unsupported,
    /// `errInvalidPaletteIndex` (reader.go:23).
    #[error("bmp: invalid palette index")]
    InvalidPaletteIndex,
}

/// The colour model `bmp.DecodeConfig` reports — the only two it can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigModel {
    /// `color.RGBAModel`, for 24 *and* 32 bits per pixel. `Decode` returns an `*image.NRGBA` for
    /// the latter: the reported model does not name the type.
    Rgba,
    /// `color.Palette` built from the file's colour table, in file order.
    Palette(Vec<Color>),
}

/// `image.Config` for a BMP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
    pub model: ConfigModel,
}

/// The four values `decodeConfig` returns (reader.go:155).
struct Header {
    config: Config,
    bits_per_pixel: u16,
    top_down: bool,
    allow_alpha: bool,
}

/// `readUint16` (reader.go:25).
fn read_u16(b: &[u8]) -> u16 {
    u16::from(b[0]) | u16::from(b[1]) << 8
}

/// `readUint32` (reader.go:29).
fn read_u32(b: &[u8]) -> u32 {
    u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16 | u32::from(b[3]) << 24
}

/// Port of `safemath.Mul3` (internal/safemath/safemath.go) on Go's 64-bit `int`: the product, or
/// `None` for a negative argument or an overflow.
///
/// The sign guard is unreachable from `decodeConfig`, which rejects a negative width of its own
/// accord first (and a negative height has already been negated); it is kept because this is a
/// port of that function, and it is covered only by [`tests::mul3_matches_safemath`]. Only a
/// *zero* other factor makes it observable at all — with any non-zero one, reinterpreting a
/// negative `int` as a `uint64` overflows the very next multiply and `Mul3` fails anyway.
fn mul3(x: i64, y: i64, z: i64) -> Option<i64> {
    if x < 0 || y < 0 || z < 0 {
        return None;
    }
    // `bits.Mul64` twice, as Go does, so the intermediate is checked as well as the result.
    let p = u128::from(x as u64) * u128::from(y as u64);
    if p >> 64 != 0 {
        return None;
    }
    let p = (p & u128::from(u64::MAX)) * u128::from(z as u64);
    if p >> 64 != 0 {
        return None;
    }
    let lo = p as u64;
    let a = lo as i64;
    if a < 0 || a as u64 != lo {
        return None;
    }
    Some(a)
}

/// The two header reads map `io.EOF` to `io.ErrUnexpectedEOF`; the colour-table read and the pixel
/// reads deliberately do not, so a file that stops exactly on one of those boundaries answers
/// "EOF" and one that stops inside a row answers "unexpected EOF".
fn unexpected(e: ReadError) -> ReadError {
    if e == ReadError::Eof {
        ReadError::UnexpectedEof
    } else {
        e
    }
}

/// Port of `decodeConfig` (reader.go:155).
fn decode_config_r<R: Read>(r: &mut R) -> Result<Header, Error> {
    let mut b = [0u8; 1024];
    let mut top_down = false;

    let head = (FILE_HEADER_LEN + 4) as usize;
    if let (_, Some(e)) = read_full(r, &mut b[..head]) {
        return Err(unexpected(e).into());
    }
    if &b[..2] != b"BM" {
        return Err(Error::InvalidFormat);
    }
    let offset = read_u32(&b[10..14]);
    let info_len = read_u32(&b[14..18]);
    if info_len != INFO_HEADER_LEN
        && info_len != V4_INFO_HEADER_LEN
        && info_len != V5_INFO_HEADER_LEN
    {
        return Err(Error::Unsupported);
    }
    let rest = (FILE_HEADER_LEN + info_len) as usize;
    if let (_, Some(e)) = read_full(r, &mut b[head..rest]) {
        return Err(unexpected(e).into());
    }

    let width = i64::from(read_u32(&b[18..22]) as i32);
    let mut height = i64::from(read_u32(&b[22..26]) as i32);
    if height < 0 {
        height = -height;
        top_down = true;
    }
    // `width < 0` is what this catches; `height < 0` cannot hold any more, because Go's `int` is
    // 64 bits wide and even `-int32::MIN` fits in it. The check is kept where Go has it.
    if width < 0 || height < 0 {
        return Err(Error::Unsupported);
    }
    if (width == 0) != (height == 0) {
        // We'll take 0x0, but Nx0 or 0xN is suspicious.
        return Err(Error::Unsupported);
    }
    // Check that the image fits in memory, conservatively assuming four bytes per pixel.
    if mul3(width, height, 4).is_none() {
        return Err(Error::Unsupported);
    }

    let planes = read_u16(&b[26..28]);
    let bpp = read_u16(&b[28..30]);
    let mut compression = read_u32(&b[30..34]);
    // BI_BITFIELDS whose masks are exactly the ones a compression of 0 implies is a compression of
    // 0. The masks live at bytes 54..70, which only a V4 or V5 header is long enough to hold — in
    // a 40-byte header those bytes are the zeroes `b` was created with, so the test cannot pass.
    if compression == 3
        && info_len > INFO_HEADER_LEN
        && read_u32(&b[54..58]) == 0x00ff_0000
        && read_u32(&b[58..62]) == 0x0000_ff00
        && read_u32(&b[62..66]) == 0x0000_00ff
        && read_u32(&b[66..70]) == 0xff00_0000
    {
        compression = 0;
    }
    if planes != 1 || compression != 0 {
        return Err(Error::Unsupported);
    }

    match bpp {
        1 | 2 | 4 | 8 => {
            let mut color_used = read_u32(&b[46..50]);
            if color_used == 0 {
                color_used = 1 << bpp;
            } else if color_used > (1 << bpp) {
                return Err(Error::Unsupported);
            }
            if offset != FILE_HEADER_LEN + info_len + color_used * 4 {
                return Err(Error::Unsupported);
            }
            let n = color_used as usize * 4;
            if let (_, Some(e)) = read_full(r, &mut b[..n]) {
                return Err(e.into());
            }
            let palette = (0..color_used as usize)
                // BMP colour tables are BGR, and every fourth byte is padding: alpha is 0xFF
                // whatever that byte holds.
                .map(|i| Color::Rgba([b[4 * i + 2], b[4 * i + 1], b[4 * i], 0xff]))
                .collect();
            Ok(Header {
                config: Config {
                    width,
                    height,
                    model: ConfigModel::Palette(palette),
                },
                bits_per_pixel: bpp,
                top_down,
                allow_alpha: false,
            })
        }
        24 => {
            if offset != FILE_HEADER_LEN + info_len {
                return Err(Error::Unsupported);
            }
            Ok(Header {
                config: Config {
                    width,
                    height,
                    model: ConfigModel::Rgba,
                },
                bits_per_pixel: 24,
                top_down,
                allow_alpha: false,
            })
        }
        32 => {
            if offset != FILE_HEADER_LEN + info_len {
                return Err(Error::Unsupported);
            }
            // Alpha in a 32-bit BMP is only honoured from BITMAPV3HEADER on; "Windows V3" there
            // means the *earlier, smaller* BITMAPINFOHEADER, so the larger headers are the ones
            // that carry an alpha mask (reader.go:251-271). Ignoring alpha means forcing 0xFF.
            Ok(Header {
                config: Config {
                    width,
                    height,
                    model: ConfigModel::Rgba,
                },
                bits_per_pixel: 32,
                top_down,
                allow_alpha: info_len > INFO_HEADER_LEN,
            })
        }
        _ => Err(Error::Unsupported),
    }
}

/// The rows of a `height`-row image in the order the file stores them.
fn row_order(height: i64, top_down: bool) -> (i64, i64, i64) {
    if top_down {
        (0, height, 1)
    } else {
        (height - 1, -1, -1)
    }
}

/// Port of `decodePaletted` (reader.go:35): a 1, 2, 4 or 8 bit-per-pixel index stream.
fn decode_paletted<R: Read>(
    r: &mut R,
    c: &Config,
    palette: &[Color],
    top_down: bool,
    bpp: u16,
) -> Result<Image, Error> {
    let mut pix = Pixels::new(Rect::new(0, 0, c.width, c.height), 1);
    let done = |pix| {
        Ok(Image::Paletted(Paletted {
            pix,
            palette: palette.to_vec(),
        }))
    };
    if c.width == 0 || c.height == 0 {
        return done(pix);
    }
    let (y0, y1, y_delta) = row_order(c.height, top_down);

    let pixels_per_byte = 8 / i64::from(bpp);
    // Pad up to ensure each row is 4-bytes aligned.
    let bytes_per_row = ((c.width + pixels_per_byte - 1) / pixels_per_byte + 3) & !3;
    let mut b = vec![0u8; bytes_per_row as usize];
    let stride = pix.stride;
    let mask = ((1u32 << bpp) - 1) as u8;

    let mut y = y0;
    while y != y1 {
        let start = y as usize * stride;
        let row = &mut pix.pix[start..start + c.width as usize];
        if let (_, Some(e)) = read_full(r, &mut b) {
            return Err(e.into());
        }
        let (mut byte_index, mut bit_index) = (0usize, 8i32);
        for p in row.iter_mut() {
            bit_index -= i32::from(bpp);
            // In range: the loop advances `byte_index` once per `pixels_per_byte` pixels, and
            // `bytes_per_row` was rounded *up* from that same count.
            let palette_index = (b[byte_index] >> bit_index) & mask;
            if usize::from(palette_index) >= palette.len() {
                return Err(Error::InvalidPaletteIndex);
            }
            *p = palette_index;
            if bit_index == 0 {
                byte_index += 1;
                bit_index = 8;
            }
        }
        y += y_delta;
    }
    done(pix)
}

/// Port of `decodeRGB` (reader.go:75): 24 bits per pixel, into an `*image.RGBA`.
fn decode_rgb<R: Read>(r: &mut R, c: &Config, top_down: bool) -> Result<Image, Error> {
    let mut pix = Pixels::new(Rect::new(0, 0, c.width, c.height), 4);
    if c.width == 0 || c.height == 0 {
        return Ok(Image::Rgba(pix));
    }
    // There are 3 bytes per pixel, and each row is 4-byte aligned.
    let mut b = vec![0u8; ((3 * c.width + 3) & !3) as usize];
    let (y0, y1, y_delta) = row_order(c.height, top_down);
    let stride = pix.stride;
    let mut y = y0;
    while y != y1 {
        if let (_, Some(e)) = read_full(r, &mut b) {
            return Err(e.into());
        }
        let start = y as usize * stride;
        let p = &mut pix.pix[start..start + c.width as usize * 4];
        for (px, src) in p.chunks_exact_mut(4).zip(b.chunks_exact(3)) {
            // BMP images are stored in BGR order rather than RGB order.
            px[0] = src[2];
            px[1] = src[1];
            px[2] = src[0];
            px[3] = 0xff;
        }
        y += y_delta;
    }
    Ok(Image::Rgba(pix))
}

/// Port of `decodeNRGBA` (reader.go:104): 32 bits per pixel, into an `*image.NRGBA`. The row is
/// read straight into the image and fixed up in place, exactly as Go does.
fn decode_nrgba<R: Read>(
    r: &mut R,
    c: &Config,
    top_down: bool,
    allow_alpha: bool,
) -> Result<Image, Error> {
    let mut pix = Pixels::new(Rect::new(0, 0, c.width, c.height), 4);
    if c.width == 0 || c.height == 0 {
        return Ok(Image::Nrgba(pix));
    }
    let (y0, y1, y_delta) = row_order(c.height, top_down);
    let stride = pix.stride;
    let mut y = y0;
    while y != y1 {
        let start = y as usize * stride;
        let p = &mut pix.pix[start..start + c.width as usize * 4];
        if let (_, Some(e)) = read_full(r, p) {
            return Err(e.into());
        }
        for px in p.chunks_exact_mut(4) {
            // BMP images are stored in BGRA order rather than RGBA order.
            px.swap(0, 2);
            if !allow_alpha {
                px[3] = 0xff;
            }
        }
        y += y_delta;
    }
    Ok(Image::Nrgba(pix))
}

/// Port of `bmp.Decode` (reader.go:131) as `image.Decode` calls it: over a `bufio.Reader` on the
/// whole input.
///
/// Allocates the full image from the header's dimensions; see the module docs.
pub fn decode(data: &[u8]) -> Result<Image, Error> {
    let mut r = BufReader::new(BytesReader::new(data));
    let h = decode_config_r(&mut r)?;
    match (h.bits_per_pixel, &h.config.model) {
        (1 | 2 | 4 | 8, ConfigModel::Palette(p)) => {
            decode_paletted(&mut r, &h.config, p, h.top_down, h.bits_per_pixel)
        }
        (24, _) => decode_rgb(&mut r, &h.config, h.top_down),
        (32, _) => decode_nrgba(&mut r, &h.config, h.top_down, h.allow_alpha),
        // Go's `panic("unreachable")`: `decodeConfig` succeeds only with one of the bit depths
        // above, and pairs each palette depth with a `color.Palette`.
        _ => Err(Error::Unsupported),
    }
}

/// Port of `bmp.DecodeConfig` (reader.go:150) as `image.DecodeConfig` calls it.
pub fn decode_config(data: &[u8]) -> Result<Config, Error> {
    let mut r = BufReader::new(BytesReader::new(data));
    decode_config_r(&mut r).map(|h| h.config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul3_matches_safemath() {
        assert_eq!(mul3(3, 5, 4), Some(60));
        assert_eq!(mul3(0, 0, 4), Some(0));
        assert_eq!(mul3(-1, 5, 4), None);
        // A negative argument beside a *zero* one is the only case where the sign guard decides
        // the answer: `uint64(-1) * 5` overflows the first Mul64 and would fail anyway, while
        // `uint64(-1) * 0` is a clean zero that Go still refuses.
        assert_eq!(mul3(-1, 0, 4), None);
        assert_eq!(mul3(0, -1, 4), None);
        assert_eq!(mul3(0, 5, -1), None);
        // The pair the reader actually meets: 2^31-1 squared fits in a u64, times four does not
        // fit in a positive Go int.
        assert_eq!(mul3(0x7fff_ffff, 0x7fff_ffff, 4), None);
        assert_eq!(mul3(0x7fff_ffff, 1, 4), Some(0x1_ffff_fffc));
        // The first Mul64 overflowing is its own branch.
        assert_eq!(mul3(1 << 40, 1 << 40, 1), None);
        // Exactly the largest positive int, and one more.
        assert_eq!(mul3(i64::MAX, 1, 1), Some(i64::MAX));
        assert_eq!(mul3(i64::MAX, 2, 1), None);
    }

    #[test]
    fn a_short_or_wrong_signature_is_not_a_bmp() {
        assert_eq!(decode(b"").err(), Some(ReadError::UnexpectedEof.into()));
        assert_eq!(
            decode(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some("unexpected EOF")
        );
        assert_eq!(
            decode(b"MB0123456789abcdef01").err(),
            Some(Error::InvalidFormat)
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
            ConfigModel::Rgba => json!("rgba"),
            ConfigModel::Palette(p) => {
                json!({ "palette": p.iter().map(palette_entry).collect::<Vec<_>>() })
            }
        }
    }

    /// Every file of the oracle's BMP corpus through `bmp.DecodeConfig` and `bmp.Decode`: the
    /// dimensions and colour model, and the decoded image's Go type, geometry, stride and pixels —
    /// or Go's error text, byte for byte.
    #[test]
    fn decode_matches_go_on_every_corpus_file() {
        let cases = fixture("bmp")["decode"].as_array().unwrap();
        let mut checked = 0;
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            let want = &c["direct"];
            let config = match decode_config(&data) {
                Ok(cfg) => {
                    json!({ "w": cfg.width, "h": cfg.height, "model": model_json(&cfg.model) })
                }
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(config, want["config"], "{name}: config");
            let image = match decode(&data) {
                Ok(m) => describe(&m),
                Err(e) => json!({ "err": e.to_string() }),
            };
            assert_eq!(image, want["image"], "{name}: image");
            checked += 1;
        }
        assert!(checked > 200, "{checked}");
    }

    /// Every error text the corpus can produce is produced by something in it. A corpus that has
    /// drifted into testing only the happy path fails here rather than quietly.
    #[test]
    fn the_corpus_reaches_every_error_and_every_pixel_loop() {
        use std::collections::HashSet;
        let mut errors = HashSet::new();
        let mut types = HashSet::new();
        for c in fixture("bmp")["decode"].as_array().unwrap() {
            let img = &c["direct"]["image"];
            match img["err"].as_str() {
                Some(e) => {
                    errors.insert(e.to_owned());
                }
                None => {
                    types.insert(img["type"].as_str().unwrap_or_default().to_owned());
                }
            }
            if let Some(e) = c["direct"]["config"]["err"].as_str() {
                errors.insert(e.to_owned());
            }
        }
        for want in [
            "EOF",
            "unexpected EOF",
            "bmp: invalid format",
            "bmp: unsupported BMP image",
            "bmp: invalid palette index",
        ] {
            assert!(errors.contains(want), "no case answers {want:?}");
        }
        assert_eq!(
            types,
            ["paletted", "rgba", "nrgba"]
                .into_iter()
                .map(str::to_owned)
                .collect::<HashSet<_>>()
        );
    }

    /// The registry's answer for every file `image.Decode` sniffs as a BMP is the decoder's own
    /// with the format name added: the `bufio.Reader` `image.Decode` interposes changes nothing,
    /// which is why [`decode`] may read through one. Files the magic does not match are
    /// `image.ErrFormat` before the decoder is ever called.
    #[test]
    fn the_registry_adds_only_the_format_name() {
        let (mut sniffed, mut unsniffed) = (0, 0);
        for c in fixture("bmp")["decode"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let data = b64(c["b64"].as_str().unwrap());
            if crate::format::sniff(&data) != Some("bmp") {
                assert_eq!(c["config"]["err"], crate::format::ERR_FORMAT, "{name}");
                assert_eq!(c["image"]["err"], crate::format::ERR_FORMAT, "{name}");
                unsniffed += 1;
                continue;
            }
            for key in ["config", "image"] {
                let mut want = c["direct"][key].clone();
                if want.get("err").is_none() {
                    want["format"] = json!("bmp");
                }
                assert_eq!(c[key], want, "{name}: registry {key}");
            }
            sniffed += 1;
        }
        assert!(sniffed > 200, "{sniffed}");
        assert!(unsniffed >= 5, "{unsniffed}");
    }
}
