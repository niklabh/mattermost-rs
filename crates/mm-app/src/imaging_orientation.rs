//! Port of Mattermost's `channels/app/imaging/orientation.go`: `GetImageOrientation`, its
//! `bufReadSeeker`, and the EXIF orientation constants `MakeImageUpright` switches on.
//!
//! The EXIF walk itself is `goimage::exif` (a port of `github.com/bep/imagemeta`); this module is
//! the Mattermost wrapper around it: which format strings are accepted, what an error turns into,
//! and — the part a caller would get wrong — the reader. `preprocessImage` hands in an
//! `io.MultiReader` (not seekable), which `GetImageOrientation` wraps in a `bufReadSeeker` that
//! seeks forward by reading ahead and fails where a `bytes.Reader` would not; `prepareImage`,
//! `AdjustImage` and `DoUploadFileExpectModification` hand in a seekable reader. The two can
//! answer differently for the same bytes, so the caller names which one it is porting.

use goimage::exif::{self, BytesReader, ReadError, ReadSeek, Whence};

/// EXIF orientations (orientation.go:18-37), as `MakeImageUpright` numbers them.
pub const UPRIGHT: i64 = 1;
pub const UPRIGHT_MIRRORED: i64 = 2;
pub const UPSIDE_DOWN: i64 = 3;
pub const UPSIDE_DOWN_MIRRORED: i64 = 4;
pub const ROTATED_CW_MIRRORED: i64 = 5;
pub const ROTATED_CCW: i64 = 6;
pub const ROTATED_CCW_MIRRORED: i64 = 7;
pub const ROTATED_CW: i64 = 8;

/// `maxExifScanSize` (orientation.go:70).
const MAX_EXIF_SCAN_SIZE: i64 = 10 * 1024 * 1024;

/// Port of `bufReadSeeker` (orientation.go:63-130) over the file's bytes, which the underlying
/// `io.Reader` delivers sequentially: reads are buffered so backward seeks work, a forward seek
/// past the buffer reads ahead (and stops at the end with `io.EOF`), and nothing past 10 MiB is
/// ever read.
pub struct BufReadSeeker<'a> {
    r: BytesReader<'a>,
    buf: Vec<u8>,
    pos: i64,
}

impl<'a> BufReadSeeker<'a> {
    /// `&bufReadSeeker{r: input}` over a stream yielding `data`.
    pub fn new(data: &'a [u8]) -> BufReadSeeker<'a> {
        BufReadSeeker {
            r: BytesReader::new(data),
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl ReadSeek for BufReadSeeker<'_> {
    /// orientation.go:73.
    fn read(&mut self, p: &mut [u8]) -> Result<usize, ReadError> {
        let len = self.buf.len() as i64;
        if self.pos < len {
            let start = self.pos as usize;
            let n = p.len().min(self.buf.len() - start);
            p[..n].copy_from_slice(&self.buf[start..start + n]);
            self.pos += n as i64;
            return Ok(n);
        }
        let remaining = MAX_EXIF_SCAN_SIZE - len;
        if remaining <= 0 {
            return Err(ReadError::Other(format!(
                "read exceeded {MAX_EXIF_SCAN_SIZE}-byte scan limit"
            )));
        }
        let limit = p.len().min(remaining as usize);
        let n = self.r.read(&mut p[..limit])?;
        self.buf.extend_from_slice(&p[..n]);
        self.pos += n as i64;
        Ok(n)
    }

    /// orientation.go:94.
    fn seek(&mut self, offset: i64, whence: Whence) -> Result<i64, ReadError> {
        let new_pos = match whence {
            Whence::Start => offset,
            Whence::Current => self.pos + offset,
            Whence::End => {
                return Err(ReadError::Other("seek: unsupported whence 2".to_owned()));
            }
        };
        if new_pos < 0 {
            return Err(ReadError::Other(format!(
                "seek: negative position {new_pos}"
            )));
        }
        if new_pos <= self.buf.len() as i64 {
            self.pos = new_pos;
            return Ok(self.pos);
        }
        if new_pos > MAX_EXIF_SCAN_SIZE {
            return Err(ReadError::Other(format!(
                "seek: target {new_pos} exceeds {MAX_EXIF_SCAN_SIZE}-byte scan limit"
            )));
        }
        // Read ahead in place: io.ReadFull into the grown tail, keeping whatever arrived.
        let old_len = self.buf.len();
        self.buf.resize(new_pos as usize, 0);
        let mut n = 0;
        let mut err = None;
        while old_len + n < self.buf.len() {
            match self.r.read(&mut self.buf[old_len + n..]) {
                Ok(k) => n += k,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        self.buf.truncate(old_len + n);
        self.pos = self.buf.len() as i64;
        match err {
            None => Ok(self.pos),
            // ReadFull: EOF after some bytes is ErrUnexpectedEOF, which Seek reports as io.EOF;
            // EOF before any is io.EOF itself.
            Some(ReadError::Eof) => Err(ReadError::Eof),
            Some(e) => Err(e),
        }
    }
}

/// What the caller hands `GetImageOrientation`.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// An `io.ReadSeeker` (a `bytes.Reader` or a multipart file): used as is.
    Seeker(&'a [u8]),
    /// A plain `io.Reader` (preprocessImage's `io.MultiReader`): wrapped in a [`BufReadSeeker`].
    Stream(&'a [u8]),
}

/// What `GetImageOrientation` returned: the orientation, and its error's text when it returned
/// one (the orientation is then always [`UPRIGHT`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub orientation: i64,
    pub err: Option<String>,
}

/// A format `GetImageOrientation` accepts whose EXIF walk is not ported (TIFF, WebP): the caller
/// cannot know Go's answer and must hand the request to Go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreproducible(pub &'static str);

/// Port of `GetImageOrientation` (orientation.go:132).
pub fn get_image_orientation(input: Input, format: &str) -> Result<Outcome, Unreproducible> {
    let format = format.strip_prefix("image/").unwrap_or(format);
    let fmt = match format {
        "jpeg" => exif::Format::Jpeg,
        "png" => exif::Format::Png,
        "tiff" => return Err(Unreproducible("the EXIF walk over a TIFF is not ported")),
        "webp" => return Err(Unreproducible("the EXIF walk over a WebP is not ported")),
        other => {
            return Ok(Outcome {
                orientation: UPRIGHT,
                err: Some(format!("unsupported image format: {other}")),
            });
        }
    };
    let res = match input {
        Input::Seeker(data) => exif::decode_orientation(&mut BytesReader::new(data), fmt),
        Input::Stream(data) => exif::decode_orientation(&mut BufReadSeeker::new(data), fmt),
    };
    Ok(match res {
        Ok(v) => Outcome {
            orientation: v.map_or(UPRIGHT, i64::from),
            err: None,
        },
        Err(e) => Outcome {
            orientation: UPRIGHT,
            err: Some(format!("failed to decode exif data: {e}")),
        },
    })
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine as _;

    /// The case's bytes; the `pad` recipe inserts an `abCD` chunk of that many zero bytes after
    /// the PNG's IHDR, as the oracle did (`withPNGChunks(pngChunkBytes("abCD", …), …)`).
    fn case_bytes(c: &serde_json::Value) -> Vec<u8> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(c["b64"].as_str().unwrap())
            .unwrap();
        let Some(pad) = c["pad"].as_u64() else {
            return data;
        };
        let mut body = b"abCD".to_vec();
        body.resize(4 + pad as usize, 0);
        let mut out = data[..33].to_vec();
        out.extend_from_slice(&(pad as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&goimage::hash::crc32_ieee(&body).to_be_bytes());
        out.extend_from_slice(&data[33..]);
        out
    }

    /// Every case of the oracle, through both reader shapes. The `png_padded_*` cases are the
    /// ones where the shapes part: an eXIf behind a chunk ending past the 10 MiB scan limit is
    /// found through a `bytes.Reader` and lost through the `bufReadSeeker`.
    #[test]
    fn every_case_matches_go_in_both_reader_modes() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/behaviour_imaging_exif.json"
        ))
        .unwrap();
        let fx: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut n = 0;
        let mut forwarded = Vec::new();
        for c in fx["cases"].as_array().unwrap() {
            let data = case_bytes(c);
            let format = c["format"].as_str().unwrap();
            for (mode, input) in [
                ("seeker", Input::Seeker(&data)),
                ("stream", Input::Stream(&data)),
            ] {
                let want = &c[mode];
                match get_image_orientation(input, format) {
                    Ok(got) => {
                        assert_eq!(
                            (got.orientation, got.err.is_some()),
                            (
                                want["orientation"].as_i64().unwrap(),
                                want["err"].as_bool().unwrap()
                            ),
                            "{} {mode} {got:?}",
                            c["name"]
                        );
                        n += 1;
                    }
                    Err(Unreproducible(_)) => forwarded.push((c["name"].to_string(), want.clone())),
                }
            }
        }
        assert!(n > 180, "{n}");
        // Only the two formats whose walks are not ported; Go answered 1 with an error for both
        // in this corpus (the bytes are a JPEG, so the TIFF and WebP walks reject them).
        assert_eq!(forwarded.len(), 4, "{forwarded:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_seek_reads_ahead_and_clamps_at_the_end() {
        let data = b"0123456789";
        let mut r = BufReadSeeker::new(data);
        assert_eq!(r.seek(4, Whence::Start), Ok(4));
        let mut b = [0u8; 2];
        assert_eq!(r.read(&mut b), Ok(2));
        assert_eq!(&b, b"45");
        // Past the end: EOF, and the position is the end of what exists.
        assert_eq!(r.seek(50, Whence::Start), Err(ReadError::Eof));
        assert_eq!(r.seek(0, Whence::Current), Ok(10));
        // Backwards within the buffer is free.
        assert_eq!(r.seek(-10, Whence::Current), Ok(0));
        assert!(r.seek(-1, Whence::Current).is_err());
        assert!(r.seek(0, Whence::End).is_err());
    }

    #[test]
    fn the_scan_limit_refuses_seeks_and_reads_past_ten_mib() {
        let data = vec![0u8; (MAX_EXIF_SCAN_SIZE + 10) as usize];
        let mut r = BufReadSeeker::new(&data);
        assert!(matches!(
            r.seek(MAX_EXIF_SCAN_SIZE + 1, Whence::Start),
            Err(ReadError::Other(_))
        ));
        assert_eq!(r.seek(0, Whence::Current), Ok(0));
        assert_eq!(
            r.seek(MAX_EXIF_SCAN_SIZE, Whence::Start),
            Ok(MAX_EXIF_SCAN_SIZE)
        );
        assert_eq!(
            r.read(&mut [0u8; 4]),
            Err(ReadError::Other(
                "read exceeded 10485760-byte scan limit".to_owned()
            ))
        );
    }

    #[test]
    fn unsupported_formats_are_upright_with_an_error_and_mime_types_are_cut() {
        let o = get_image_orientation(Input::Seeker(b""), "image/gif").unwrap();
        assert_eq!(o.orientation, UPRIGHT);
        assert_eq!(o.err.as_deref(), Some("unsupported image format: gif"));
        assert!(get_image_orientation(Input::Stream(b""), "image/webp").is_err());
        assert_eq!(
            get_image_orientation(Input::Stream(b""), "image/jpeg").unwrap(),
            Outcome {
                orientation: UPRIGHT,
                err: None
            }
        );
    }
}
