//! Port of `image.Decode` and `image.DecodeConfig` (image/format.go) over the registry a
//! Mattermost server links: `png`, `jpeg` and `gif` from the standard library, `bmp`, `tiff` and
//! `webp` from `golang.org/x/image` (registered by `channels/app/imaging/decode.go`).
//!
//! `sniff` picks the first registered format whose magic prefix matches, `?` being a wildcard
//! byte; no match is `image.ErrFormat`. PNG, JPEG, GIF and BMP are decoded here. TIFF and
//! WebP are **recognised but not decoded** — [`DecodeError::NotPorted`] names the format so a
//! caller can hand the request to Go instead of guessing at an answer.

use crate::image::Image;

/// `image.ErrFormat`'s text.
pub const ERR_FORMAT: &str = "image: unknown format";

/// One registered decoder: `image.RegisterFormat`'s name and magic.
struct Format {
    name: &'static str,
    magic: &'static [u8],
}

/// The six registrations, magic strings copied from each `RegisterFormat` call. No two can match
/// the same bytes, so their order does not change an answer.
const FORMATS: &[Format] = &[
    // image/png/reader.go — `pngHeader`.
    Format {
        name: "png",
        magic: b"\x89PNG\r\n\x1a\n",
    },
    // image/jpeg/reader.go.
    Format {
        name: "jpeg",
        magic: b"\xff\xd8",
    },
    // image/gif/reader.go.
    Format {
        name: "gif",
        magic: b"GIF8?a",
    },
    // golang.org/x/image/bmp/reader.go.
    Format {
        name: "bmp",
        magic: b"BM????\x00\x00\x00\x00",
    },
    // golang.org/x/image/tiff/reader.go — little- and big-endian headers.
    Format {
        name: "tiff",
        magic: b"II\x2a\x00",
    },
    Format {
        name: "tiff",
        magic: b"MM\x00\x2a",
    },
    // golang.org/x/image/webp/decode.go.
    Format {
        name: "webp",
        magic: b"RIFF????WEBPVP8",
    },
];

/// Port of `image.sniff` (image/format.go:88) with `match`: the name of the first format whose
/// magic prefixes `data`, or `None` for `ErrFormat`.
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    FORMATS
        .iter()
        .find(|f| {
            data.len() >= f.magic.len()
                && f.magic.iter().zip(data).all(|(m, b)| *m == b'?' || m == b)
        })
        .map(|f| f.name)
}

/// Why `image.Decode` or `image.DecodeConfig` produced no answer here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Go's error, with Go's text: `image.ErrFormat`, or whatever the format's decoder returned.
    #[error("{0}")]
    Go(String),
    /// The bytes are a registered format this crate does not decode. Go has an answer; this
    /// crate does not claim to know it.
    #[error("the {0} decoder is not ported")]
    NotPorted(&'static str),
}

/// `image.Config`: dimensions only — the colour model is the decoders' own business
/// ([`crate::png::decode_config`], [`crate::jpeg::decode_config`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i64,
    pub height: i64,
}

/// Port of `image.DecodeConfig` (image/format.go:114): the format name and the dimensions.
pub fn decode_config(data: &[u8]) -> Result<(Config, &'static str), DecodeError> {
    match sniff(data) {
        None => Err(DecodeError::Go(ERR_FORMAT.to_owned())),
        Some("png") => crate::png::decode_config(data)
            .map(|c| {
                (
                    Config {
                        width: c.width,
                        height: c.height,
                    },
                    "png",
                )
            })
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("jpeg") => crate::jpeg::decode_config(data)
            .map(|c| {
                (
                    Config {
                        width: c.width,
                        height: c.height,
                    },
                    "jpeg",
                )
            })
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("gif") => crate::gif::decode_config(data)
            .map(|c| {
                (
                    Config {
                        width: c.width,
                        height: c.height,
                    },
                    "gif",
                )
            })
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("bmp") => crate::bmp::decode_config(data)
            .map(|c| {
                (
                    Config {
                        width: c.width,
                        height: c.height,
                    },
                    "bmp",
                )
            })
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some(other) => Err(DecodeError::NotPorted(other)),
    }
}

/// Port of `image.Decode` (image/format.go:100): the image and the format name.
///
/// The decoders allocate from the dimensions the header declares, exactly as Go's do; a caller
/// facing untrusted input checks those against its resolution limit first (Mattermost's
/// `imaging.Decoder.Decode` does, and `mm_app::imaging` ports it).
pub fn decode(data: &[u8]) -> Result<(Image, &'static str), DecodeError> {
    match sniff(data) {
        None => Err(DecodeError::Go(ERR_FORMAT.to_owned())),
        Some("png") => crate::png::decode(data)
            .map(|m| (m, "png"))
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("jpeg") => crate::jpeg::decode(data)
            .map(|m| (m, "jpeg"))
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("gif") => crate::gif::decode(data)
            .map(|m| (m, "gif"))
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some("bmp") => crate::bmp::decode(data)
            .map(|m| (m, "bmp"))
            .map_err(|e| DecodeError::Go(e.to_string())),
        Some(other) => Err(DecodeError::NotPorted(other)),
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::testsupport::{b64, describe, fixture};

    /// How many corpus files across all the stages below match no registered magic at all, so
    /// `image.Decode` answers `ErrFormat` before any decoder is reached. Pinned rather than
    /// derived: a codec whose magic stopped matching would otherwise hide inside this loop.
    const UNSNIFFABLE: usize = 23;

    /// Every file of the decode corpora of every ported codec, through `image.DecodeConfig` and
    /// `image.Decode` as a whole — including the inputs no magic matches, which the per-codec
    /// suites skip because the registry answers them.
    #[test]
    fn the_registry_answers_every_corpus_file_as_go_does() {
        let mut unknown = 0;
        for stage in ["png", "jpeg", "gif", "bmp"] {
            for c in fixture(stage)["decode"].as_array().unwrap() {
                let data = b64(c["b64"].as_str().unwrap());
                let name = &c["name"];
                match decode_config(&data) {
                    Ok((cfg, format)) => {
                        assert_eq!(c["config"]["w"], cfg.width, "{stage} {name}");
                        assert_eq!(c["config"]["h"], cfg.height, "{stage} {name}");
                        assert_eq!(c["config"]["format"], format, "{stage} {name}");
                    }
                    Err(e) => assert_eq!(c["config"]["err"], e.to_string(), "{stage} {name}"),
                }
                match decode(&data) {
                    Ok((m, format)) => {
                        let mut d = describe(&m);
                        d["format"] = format.into();
                        assert_eq!(c["image"], d, "{stage} {name}");
                    }
                    Err(e) => {
                        if e.to_string() == ERR_FORMAT {
                            unknown += 1;
                        }
                        assert_eq!(c["image"]["err"], e.to_string(), "{stage} {name}");
                    }
                }
            }
        }
        assert_eq!(unknown, UNSNIFFABLE, "the corpora's no-magic inputs");
    }

    /// The registered formats this crate does not decode are named, not refused.
    #[test]
    fn unported_formats_are_recognised() {
        for (data, name) in [
            (&b"II\x2a\x00"[..], "tiff"),
            (b"MM\x00\x2a", "tiff"),
            (b"RIFF\x00\x00\x00\x00WEBPVP8L", "webp"),
        ] {
            assert_eq!(decode(data).err(), Some(DecodeError::NotPorted(name)));
            assert_eq!(
                decode_config(data).err(),
                Some(DecodeError::NotPorted(name))
            );
        }
        // `BM` alone is short of the magic: ErrFormat, not bmp.
        assert_eq!(
            decode(b"BM\x00\x00").err(),
            Some(DecodeError::Go(ERR_FORMAT.to_owned()))
        );
    }
}
