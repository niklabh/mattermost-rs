//! Port of `image.Decode` and `image.DecodeConfig` (image/format.go) over the registry a
//! Mattermost server links: `png`, `jpeg` and `gif` from the standard library, `bmp`, `tiff` and
//! `webp` from `golang.org/x/image` (registered by `channels/app/imaging/decode.go`).
//!
//! `sniff` picks the first registered format whose magic prefix matches, `?` being a wildcard
//! byte; no match is `image.ErrFormat`. Every format is decoded here. The one answer this crate
//! still declines is a **lossy WebP carrying an alpha chunk**, which Go returns as an
//! `*image.NYCbCrA` — a type [`crate::image::Image`] does not model — and which
//! [`DecodeError::NotPorted`] names so a caller can hand that request to Go rather than guess.

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

/// Whether a WEBP's canvas declares an alpha channel, which is the whole of what this crate
/// declines to answer.
///
/// Go decodes a **lossy** frame carrying an `ALPH` chunk into an `*image.NYCbCrA`, and
/// [`Image`] has no variant for that. The test has to be taken from the *header*, not from the
/// decode, because the caller that matters — `UploadFileTask` — measures the image with
/// `DecodeConfig` and decodes it only after it has written the file. A hand-over decided by the
/// decode would arrive after the write.
///
/// So the config's own model is the evidence, and it over-forwards: a VP8X canvas with the alpha
/// bit set whose frame turns out to be *lossless* decodes to an `*image.NRGBA` this crate can
/// produce, and is handed to Go anyway. That is a deliberate over-approximation of a gap, not a
/// missing branch — see [D-650].
fn webp_is_nycbcra(data: &[u8]) -> bool {
    matches!(
        crate::webp::decode_config(data),
        Ok(c) if c.model == crate::webp::ConfigModel::Nycbcra
    )
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
        Some("webp") if webp_is_nycbcra(data) => Err(DecodeError::NotPorted("webp")),
        Some("webp") => match crate::webp::decode_config(data) {
            Ok(c) => Ok((
                Config {
                    width: c.width,
                    height: c.height,
                },
                "webp",
            )),
            Err(e) => Err(DecodeError::Go(e.to_string())),
        },
        Some("tiff") => crate::tiff::decode_config(data)
            .map(|c| {
                (
                    Config {
                        width: c.width,
                        height: c.height,
                    },
                    "tiff",
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
        Some("webp") => {
            // The same hand-over `decode_config` makes, taken from the same evidence, so the two
            // halves never disagree — see the comment there.
            if webp_is_nycbcra(data) {
                return Err(DecodeError::NotPorted("webp"));
            }
            crate::webp::decode(data)
                .map(|m| (m, "webp"))
                .map_err(|e| match e {
                    crate::webp::Error::NycbcraUnsupported => DecodeError::NotPorted("webp"),
                    e => DecodeError::Go(e.to_string()),
                })
        }
        Some("tiff") => crate::tiff::decode(data)
            .map(|m| (m, "tiff"))
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
    const UNSNIFFABLE: usize = 80;

    /// How many corpus files declare a WEBP canvas with an alpha channel, which is the one answer
    /// the registry hands to Go — see [`webp_is_nycbcra`].
    const NYCBCRA_CANVASES: usize = 49;

    /// Every file of the decode corpora of every ported codec, through `image.DecodeConfig` and
    /// `image.Decode` as a whole — including the inputs no magic matches, which the per-codec
    /// suites skip because the registry answers them.
    #[test]
    fn the_registry_answers_every_corpus_file_as_go_does() {
        let (mut unknown, mut handed_over) = (0, 0);
        for stage in ["png", "jpeg", "gif", "bmp", "tiff", "webp"] {
            for c in fixture(stage)["decode"].as_array().unwrap() {
                let data = b64(c["b64"].as_str().unwrap());
                let name = &c["name"];
                // The webp stage records `webp.DecodeConfig`/`webp.Decode` **directly** — the
                // generic entry points sniff first and would answer `ErrFormat` for every
                // container error the corpus exists to pin — and carries `sniff` beside them for
                // what the registry itself said. So for that stage the registry is checked
                // against `sniff`, and only a case it routed to the decoder is compared further.
                if let Some(sniffed) = c["sniff"].as_str()
                    && sniffed != "webp"
                {
                    let want = Some(DecodeError::Go(sniffed.to_owned()));
                    assert_eq!(decode_config(&data).err(), want, "{stage} {name}");
                    assert_eq!(decode(&data).err(), want, "{stage} {name}");
                    if sniffed == ERR_FORMAT {
                        unknown += 1;
                    }
                    continue;
                }
                if decode_config(&data).err() == Some(DecodeError::NotPorted("webp")) {
                    assert_eq!(
                        decode(&data).err(),
                        Some(DecodeError::NotPorted("webp")),
                        "{stage} {name}: the two halves must hand over together"
                    );
                    handed_over += 1;
                    continue;
                }
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
        assert_eq!(
            handed_over, NYCBCRA_CANVASES,
            "the corpora's WEBP hand-overs"
        );
    }

    /// The one answer this crate declines: a lossy WebP with an alpha chunk, which Go returns as
    /// an `*image.NYCbCrA`. Both halves of the registry hand it over, so a caller cannot measure
    /// it here and then fail to decode it.
    #[test]
    fn unported_formats_are_recognised() {
        for c in fixture("webp")["decode"].as_array().unwrap() {
            if c["config"]["model"] != "nycbcra" {
                continue;
            }
            let data = b64(c["b64"].as_str().unwrap());
            assert_eq!(
                decode_config(&data).err(),
                Some(DecodeError::NotPorted("webp")),
                "{}",
                c["name"]
            );
            assert_eq!(
                decode(&data).err(),
                Some(DecodeError::NotPorted("webp")),
                "{}",
                c["name"]
            );
        }
        // `BM` alone is short of the magic: ErrFormat, not bmp.
        assert_eq!(
            decode(b"BM\x00\x00").err(),
            Some(DecodeError::Go(ERR_FORMAT.to_owned()))
        );
    }
}
