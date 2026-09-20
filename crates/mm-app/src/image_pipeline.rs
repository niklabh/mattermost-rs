//! Port of Mattermost's `channels/app/imaging` call surface — `Decoder`, `Encoder`, `Fit`,
//! `FillCenter`, `GenerateThumbnail`, `GeneratePreview`, `GenerateMiniPreviewImage`,
//! `MakeImageUpright` — and the constants `app/file.go` feeds them, over the byte-exact codecs and
//! resampler in `goimage`.
//!
//! These are thin in Go and thin here; what they decide is small but load-bearing: the decoder's
//! resolution guard runs **before** the pixels are allocated (so a decompression bomb is an error,
//! not an allocation), `GeneratePreview` hands back the *original* image when it is already narrow
//! enough (so a small JPEG's preview is re-encoded from its Y'CbCr planes, not from NRGBA),
//! `GenerateThumbnail` keys its axis on `width > height` alone, and the mini preview is always
//! JPEG at quality 90 whatever the source format.
//!
//! # What this does not decode
//!
//! Go decodes a lossy WebP carrying an alpha chunk into an `*image.NYCbCrA`, which
//! `goimage::image::Image` does not model. Such a canvas is recognised here and answered
//! [`PipelineError::NotPorted`]; every caller hands such a request to Go before it writes.

use std::borrow::Cow;

use goimage::format::{self, DecodeError};
use goimage::image::Image;
use goimage::imaging::{self, Anchor, LANCZOS};

use crate::imaging_orientation::{
    ROTATED_CCW, ROTATED_CCW_MIRRORED, ROTATED_CW, ROTATED_CW_MIRRORED, UPRIGHT_MIRRORED,
    UPSIDE_DOWN, UPSIDE_DOWN_MIRRORED,
};

/// `imageThumbnailWidth` (app/file.go:44).
pub const IMAGE_THUMBNAIL_WIDTH: i64 = 120;
/// `imageThumbnailHeight` (app/file.go:45).
pub const IMAGE_THUMBNAIL_HEIGHT: i64 = 100;
/// `imagePreviewWidth` (app/file.go:46).
pub const IMAGE_PREVIEW_WIDTH: i64 = 1920;
/// `miniPreviewImageWidth` / `miniPreviewImageHeight` (app/file.go:47-48).
pub const MINI_PREVIEW_IMAGE_SIZE: i64 = 16;
/// `jpegEncQuality` (app/file.go:49).
pub const JPEG_ENC_QUALITY: i32 = 90;
/// The side of a profile picture and a team icon: `profileWidthAndHeight` (app/user.go:1107) and
/// `teamIconWidthAndHeight` (app/team.go:2390).
pub const PROFILE_WIDTH_AND_HEIGHT: i64 = 128;

/// Why a pipeline step produced no image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    /// Go's error, with Go's text (including `imaging:`'s wrapping).
    #[error("{0}")]
    Go(String),
    /// A format Go decodes and this port does not: the caller hands the request to Go.
    #[error("the {0} decoder is not ported")]
    NotPorted(&'static str),
}

impl PipelineError {
    fn wrap(prefix: &str, err: DecodeError) -> PipelineError {
        match err {
            DecodeError::Go(text) => PipelineError::Go(format!("{prefix}{text}")),
            DecodeError::NotPorted(name) => PipelineError::NotPorted(name),
        }
    }
}

/// `image.Config` as `Decoder.DecodeConfig` reports it: the dimensions and the format name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageConfig {
    pub width: i64,
    pub height: i64,
    pub format: &'static str,
}

/// Port of `imaging.Decoder.DecodeConfig` (imaging/decode.go:186): `image.DecodeConfig` with its
/// error wrapped.
pub fn decode_config(data: &[u8]) -> Result<ImageConfig, PipelineError> {
    format::decode_config(data)
        .map(|(c, f)| ImageConfig {
            width: c.width,
            height: c.height,
            format: f,
        })
        .map_err(|e| PipelineError::wrap("imaging: failed to decode image config: ", e))
}

/// Port of `imaging.Decoder.Decode` (imaging/decode.go:130) with `enforceResolutionLimit`: when
/// `max_resolution > 0`, the header is read first and an image declaring more pixels is refused
/// before any are decoded; a header that does not parse is left for the full decode to report.
pub fn decode(data: &[u8], max_resolution: i64) -> Result<(Image, &'static str), PipelineError> {
    if max_resolution > 0 {
        match format::decode_config(data) {
            Ok((c, _)) if exceeds_resolution(c.width, c.height, max_resolution) => {
                return Err(PipelineError::Go(format!(
                    "imaging: image resolution {}x{} exceeds the maximum allowed {max_resolution} pixels",
                    c.width, c.height
                )));
            }
            Err(DecodeError::NotPorted(name)) => return Err(PipelineError::NotPorted(name)),
            _ => {}
        }
    }
    format::decode(data).map_err(|e| PipelineError::wrap("imaging: failed to decode image: ", e))
}

/// Port of `exceedsResolution` (imaging/decode.go:122): division rather than a product, so a
/// huge declared size cannot overflow.
fn exceeds_resolution(width: i64, height: i64, max_resolution: i64) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    width > max_resolution / height
}

/// Port of `imaging.MakeImageUpright` (imaging/orientation.go:40). `Cow::Borrowed` for the
/// orientations Go returns the input unchanged for (1, and anything outside 2..=8).
pub fn make_image_upright(img: &Image, orientation: i64) -> Cow<'_, Image> {
    Cow::Owned(match orientation {
        UPRIGHT_MIRRORED => imaging::flip_h(img),
        UPSIDE_DOWN => imaging::rotate180(img),
        UPSIDE_DOWN_MIRRORED => imaging::flip_v(img),
        ROTATED_CW_MIRRORED => imaging::transpose(img),
        ROTATED_CCW => imaging::rotate270(img),
        ROTATED_CCW_MIRRORED => imaging::transverse(img),
        ROTATED_CW => imaging::rotate90(img),
        _ => return Cow::Borrowed(img),
    })
}

/// Port of `imaging.GenerateThumbnail` (imaging/preview.go:29): the axis is picked by
/// `width > height` alone — a square image is fitted to the height.
pub fn generate_thumbnail(img: &Image, target_width: i64, target_height: i64) -> Image {
    let b = img.bounds();
    if b.dx() > b.dy() {
        imaging::resize(img, target_width, 0, &LANCZOS)
    } else {
        imaging::resize(img, 0, target_height, &LANCZOS)
    }
}

/// Port of `imaging.GeneratePreview` (imaging/preview.go:16): the input itself when it is not
/// wider than `width`.
pub fn generate_preview(img: &Image, width: i64) -> Cow<'_, Image> {
    if img.bounds().dx() > width {
        Cow::Owned(imaging::resize(img, width, 0, &LANCZOS))
    } else {
        Cow::Borrowed(img)
    }
}

/// Port of `imaging.GenerateMiniPreviewImage` (imaging/preview.go:40): a Lanczos resize to
/// exactly `w`×`h`, encoded as JPEG at quality `q`.
pub fn generate_mini_preview_image(
    img: &Image,
    w: i64,
    h: i64,
    q: i32,
) -> Result<Vec<u8>, PipelineError> {
    let preview = imaging::resize(img, w, h, &LANCZOS);
    let mut out = Vec::new();
    goimage::jpeg::encode(
        &mut out,
        &preview,
        Some(goimage::jpeg::Options { quality: q }),
    )
    .map_err(|e| PipelineError::Go(format!("failed to encode image to JPEG format: {e}")))?;
    Ok(out)
}

/// Port of `imaging.FillCenter` (imaging/utils.go:141).
pub fn fill_center(img: &Image, w: i64, h: i64) -> Image {
    imaging::fill(img, w, h, Anchor::Center, &LANCZOS)
}

/// Port of `imaging.Fit` (imaging/utils.go:147).
pub fn fit(img: &Image, max_w: i64, max_h: i64) -> Image {
    imaging::fit(img, max_w, max_h, &LANCZOS)
}

/// Port of `imaging.Encoder.EncodePNG` (imaging/encode.go:77): `png.BestCompression`.
pub fn encode_png(img: &Image) -> Result<Vec<u8>, PipelineError> {
    let mut out = Vec::new();
    goimage::png::encode(
        &mut out,
        img,
        goimage::png::CompressionLevel::BestCompression,
    )
    .map_err(|e| PipelineError::Go(format!("imaging: failed to encode png: {e}")))?;
    Ok(out)
}

/// Port of `imaging.Encoder.EncodeJPEG` (imaging/encode.go:57).
pub fn encode_jpeg(img: &Image, quality: i32) -> Result<Vec<u8>, PipelineError> {
    let mut out = Vec::new();
    goimage::jpeg::encode(&mut out, img, Some(goimage::jpeg::Options { quality }))
        .map_err(|e| PipelineError::Go(format!("imaging: failed to encode jpeg: {e}")))?;
    Ok(out)
}

/// The encoder choice `postprocessImage`'s `writeImage` and `generateThumbnailImage` /
/// `generatePreviewImage` make: PNG when the **decoder** said `png`, JPEG at quality 90 for
/// everything else — whatever the file was named.
pub fn encode_derived(img: &Image, img_type: &str) -> Result<Vec<u8>, PipelineError> {
    if img_type == "png" {
        encode_png(img)
    } else {
        encode_jpeg(img, JPEG_ENC_QUALITY)
    }
}

/// The `_thumb` and `_preview` bytes for one decoded, upright image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedImages {
    pub thumbnail: Result<Vec<u8>, PipelineError>,
    pub preview: Result<Vec<u8>, PipelineError>,
}

/// The thumbnail and preview `postprocessImage` and `HandleImages` both write.
pub fn derived_images(upright: &Image, img_type: &str) -> DerivedImages {
    DerivedImages {
        thumbnail: encode_derived(
            &generate_thumbnail(upright, IMAGE_THUMBNAIL_WIDTH, IMAGE_THUMBNAIL_HEIGHT),
            img_type,
        ),
        preview: encode_derived(&generate_preview(upright, IMAGE_PREVIEW_WIDTH), img_type),
    }
}

/// The mini preview `postprocessImage` and `generateMiniPreview` store in the FileInfo row.
pub fn mini_preview(upright: &Image) -> Result<Vec<u8>, PipelineError> {
    generate_mini_preview_image(
        upright,
        MINI_PREVIEW_IMAGE_SIZE,
        MINI_PREVIEW_IMAGE_SIZE,
        JPEG_ENC_QUALITY,
    )
}

/// What `postprocessImage` stores for one upload: the two derived files and the mini preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Postprocessed {
    pub derived: DerivedImages,
    pub mini_preview: Result<Vec<u8>, PipelineError>,
}

/// Port of `UploadFileTask.postprocessImage` (app/file.go:951) minus the writes: decode the stored
/// bytes (a failure is Go's log line and nothing more — `Ok(None)`), make them upright with the
/// orientation `preprocessImage` read through the stream reader, and derive the thumbnail, the
/// preview and the mini preview. `Err` names a format this port does not decode.
///
/// CPU-bound: call it from a blocking context.
pub fn postprocess_image(
    data: &[u8],
    max_resolution: i64,
    orientation: i64,
) -> Result<Option<Postprocessed>, &'static str> {
    let (img, img_type) = match decode(data, max_resolution) {
        Ok(decoded) => decoded,
        Err(PipelineError::NotPorted(name)) => return Err(name),
        Err(err) => {
            tracing::error!(error = %err, "Unable to decode image");
            return Ok(None);
        }
    };
    let upright = make_image_upright(&img, orientation);
    Ok(Some(Postprocessed {
        derived: derived_images(&upright, img_type),
        mini_preview: mini_preview(&upright),
    }))
}

/// Port of one iteration of `App.HandleImages` (app/file.go:1161) with `prepareImage`
/// (app/file.go:1186): decode (a failure is skipped — `Ok(None)`), read the orientation through a
/// **seekable** reader with the decoder's format name, make upright, derive the thumbnail and the
/// preview. No mini preview on this path; `createPost` generates it later. `Err` names a format
/// this port does not decode.
///
/// CPU-bound: call it from a blocking context.
pub fn handle_image(
    data: &[u8],
    max_resolution: i64,
) -> Result<Option<DerivedImages>, &'static str> {
    let (img, img_type) = match decode(data, max_resolution) {
        Ok(decoded) => decoded,
        Err(PipelineError::NotPorted(name)) => return Err(name),
        Err(err) => {
            tracing::debug!(error = %err, "Failed to prepare image");
            return Ok(None);
        }
    };
    let orientation = seeker_orientation(data, img_type)?;
    let upright = make_image_upright(&img, orientation);
    Ok(Some(derived_images(&upright, img_type)))
}

/// Port of `App.generateMiniPreview`'s pixel half (app/file.go:1252): `prepareImage` over the
/// stored original, then the mini preview. `Ok(None)` when the original does not decode (Go logs
/// at debug and leaves `MiniPreview` nil); the encode failing is `Ok(Some(Err))`, which Go logs and
/// also leaves nil.
///
/// CPU-bound: call it from a blocking context.
pub fn generate_mini_preview(
    data: &[u8],
    max_resolution: i64,
) -> Result<Option<Result<Vec<u8>, PipelineError>>, &'static str> {
    let (img, img_type) = match decode(data, max_resolution) {
        Ok(decoded) => decoded,
        Err(PipelineError::NotPorted(name)) => return Err(name),
        Err(err) => {
            tracing::debug!(error = %err, "generateMiniPreview: prepareImage failed");
            return Ok(None);
        }
    };
    let orientation = seeker_orientation(data, img_type)?;
    Ok(Some(mini_preview(&make_image_upright(&img, orientation))))
}

/// `GetImageOrientation` over a seekable reader, as `prepareImage` calls it: an error is logged and
/// the orientation it came with (Upright) is used.
fn seeker_orientation(data: &[u8], img_type: &str) -> Result<i64, &'static str> {
    let outcome = crate::imaging_orientation::get_image_orientation(
        crate::imaging_orientation::Input::Seeker(data),
        img_type,
    )
    .map_err(|crate::imaging_orientation::Unreproducible(why)| why)?;
    if let Some(err) = &outcome.err {
        tracing::debug!(error = %err, "GetImageOrientation failed");
    }
    Ok(outcome.orientation)
}

#[cfg(test)]
mod go_parity {
    //! End to end against `fixtures/behaviour_imaging_pipeline.json`: each input through the
    //! exact call sequence of every Mattermost write path that stores derived image bytes. See
    //! `reference/dump/behaviour_imaging_pipeline.go` for what each column is.

    use super::*;
    use crate::imaging_orientation::{Input, get_image_orientation};
    use base64::Engine as _;
    use serde_json::Value as Json;
    use sha2::Digest as _;

    const MAX_RES: i64 = 7680 * 4320;

    fn fixture(stage: &str) -> Json {
        let path = format!(
            "{}/../../fixtures/behaviour_imaging_{stage}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn b64(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    fn sha(b: &[u8]) -> String {
        sha2::Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect()
    }

    /// Compare one encoder output (or error) against the oracle's `encoded` object.
    fn check(label: &str, got: &Result<Vec<u8>, PipelineError>, img: &Image, want: &Json) {
        match got {
            Ok(bytes) => {
                assert_eq!(
                    sha(bytes),
                    want["sha256"].as_str().unwrap_or_default(),
                    "{label}: len {} vs Go {}",
                    bytes.len(),
                    want["len"]
                );
                assert_eq!(want["w"], img.bounds().dx(), "{label} width");
                assert_eq!(want["h"], img.bounds().dy(), "{label} height");
            }
            Err(e) => assert_eq!(want["err"], e.to_string(), "{label}"),
        }
    }

    /// `GetImageOrientation`'s answer. Every format the registry recognises has a ported walk
    /// now, so the `Unreproducible` arm is unreachable and this may assert.
    fn orientation(input: Input, format: &str) -> Json {
        let o = get_image_orientation(input, format).expect("every walk is ported");
        serde_json::json!({ "orientation": o.orientation, "err": o.err.is_some() })
    }

    /// How many corpus cases reach a decoder this port does not have. Every one is a WebP, and
    /// each is a request the server hands to Go — counted, so the gap closing is visible.
    static UNDECODED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn every_pipeline_case_matches_go() {
        let png = fixture("png");
        let jpeg = fixture("jpeg");
        let gif = fixture("gif");
        let bmp = fixture("bmp");
        let tiff = fixture("tiff");
        let webp = fixture("webp");
        let exif = fixture("exif");
        let bytes_of = |c: &Json| -> Vec<u8> {
            let name = &c["name"];
            let source = match c["source"].as_str().unwrap() {
                "inline" => return b64(c["b64"].as_str().unwrap()),
                "png" => &png["decode"],
                "jpeg" => &jpeg["decode"],
                "gif" => &gif["decode"],
                "bmp" => &bmp["decode"],
                "tiff" => &tiff["decode"],
                "webp" => &webp["decode"],
                "exif" => &exif["cases"],
                other => panic!("{other}"),
            };
            let case = source
                .as_array()
                .unwrap()
                .iter()
                .find(|x| &x["name"] == name)
                .unwrap();
            b64(case["b64"].as_str().unwrap())
        };

        let fx = fixture("pipeline");
        let cases = fx["cases"].as_array().unwrap();
        // The photo-sized cases dominate; spread the cases over a few threads so the suite stays
        // fast. Each case is independent, and a failing assert fails the scope.
        let workers = 8;
        std::thread::scope(|scope| {
            for w in 0..workers {
                let bytes_of = &bytes_of;
                scope.spawn(move || {
                    for c in cases.iter().skip(w).step_by(workers) {
                        run_case(c, bytes_of(c));
                    }
                });
            }
        });
        assert!(cases.len() > 250, "{}", cases.len());
        // Every WebP is handed to Go; nothing else is.
        assert!(
            UNDECODED.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the corpus should still carry the format that forwards"
        );
    }

    fn run_case(c: &Json, data: Vec<u8>) {
        {
            let label = format!("{}/{}", c["source"], c["name"]);

            let config = decode_config(&data);
            let format = match &config {
                Ok(cfg) => {
                    assert_eq!(c["config"]["w"], cfg.width, "{label}");
                    assert_eq!(c["config"]["h"], cfg.height, "{label}");
                    assert_eq!(c["config"]["format"], cfg.format, "{label}");
                    cfg.format
                }
                // A format with no decoder here: the server forwards the request rather than
                // answering it, so there is nothing to compare past this point. The orientation
                // walk *is* ported for it, and `GetImageOrientation` is driven by the format
                // name, so take the oracle's and keep checking that half.
                Err(PipelineError::NotPorted(name)) => {
                    UNDECODED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(*name, "webp", "{label}: only WebP has no decoder");
                    // `preprocessImage` passes `DecodeConfig`'s format name and `prepareImage`
                    // passes `Decode`'s, and for a WebP whose pixels Go cannot decode the second
                    // is `""` where the first is `"webp"`. Both come off the oracle here.
                    assert_eq!(
                        c["orientation_stream"],
                        orientation(
                            Input::Stream(&data),
                            c["config"]["format"].as_str().unwrap_or_default()
                        ),
                        "{label} stream orientation"
                    );
                    assert_eq!(
                        c["orientation_seeker"],
                        orientation(
                            Input::Seeker(&data),
                            c["decode"]["format"].as_str().unwrap_or_default()
                        ),
                        "{label} seeker orientation"
                    );
                    return;
                }
                Err(e) => {
                    assert_eq!(c["config"]["err"], e.to_string(), "{label}");
                    ""
                }
            };
            let o_stream = orientation(Input::Stream(&data), format);
            assert_eq!(
                c["orientation_stream"], o_stream,
                "{label} stream orientation"
            );

            let decoded = decode(&data, MAX_RES);
            let img_type = match &decoded {
                Ok((_, t)) => {
                    assert_eq!(c["decode"]["format"], *t, "{label}");
                    *t
                }
                Err(e) => {
                    assert_eq!(c["decode"]["err"], e.to_string(), "{label}");
                    ""
                }
            };
            let o_seek = orientation(Input::Seeker(&data), img_type);
            assert_eq!(
                c["orientation_seeker"], o_seek,
                "{label} seeker orientation"
            );

            if let Ok((img, img_type)) = &decoded {
                let o = o_stream["orientation"].as_i64().unwrap();
                let up = make_image_upright(img, o);
                let thumb = generate_thumbnail(&up, 120, 100);
                check(
                    &format!("{label} upload thumb"),
                    &encode_derived(&thumb, img_type),
                    &thumb,
                    &c["upload"]["thumb"],
                );
                let prev = generate_preview(&up, 1920);
                check(
                    &format!("{label} upload preview"),
                    &encode_derived(&prev, img_type),
                    &prev,
                    &c["upload"]["preview"],
                );
                match mini_preview(&up) {
                    Ok(m) => assert_eq!(c["upload"]["mini"], b64_str(&m), "{label} mini"),
                    Err(e) => assert_eq!(c["upload"]["mini_err"], e.to_string(), "{label}"),
                }

                let o = o_seek["orientation"].as_i64().unwrap();
                let hs = make_image_upright(img, o);
                let thumb = generate_thumbnail(&hs, 120, 100);
                check(
                    &format!("{label} handle thumb"),
                    &encode_derived(&thumb, img_type),
                    &thumb,
                    &c["handle"]["thumb"],
                );
                let prev = generate_preview(&hs, 1920);
                check(
                    &format!("{label} handle preview"),
                    &encode_derived(&prev, img_type),
                    &prev,
                    &c["handle"]["preview"],
                );
                assert_eq!(
                    c["mini"],
                    b64_str(&mini_preview(&hs).unwrap()),
                    "{label} createPost mini"
                );
                let profile = fill_center(&hs, 128, 128);
                check(
                    &format!("{label} profile"),
                    &encode_png(&profile),
                    &profile,
                    &c["profile"],
                );
                check(
                    &format!("{label} brand"),
                    &encode_png(img),
                    img,
                    &c["brand"],
                );
            } else {
                assert!(c["upload"].is_null(), "{label}: Go decoded it");
            }

            // emoji: image.DecodeConfig / image.Decode directly, no guard, no orientation.
            if let Ok((cfg, _)) = format::decode_config(&data) {
                if cfg.width <= 128 && cfg.height <= 128 {
                    assert_eq!(c["emoji"]["write_through"], true, "{label} emoji");
                } else {
                    match format::decode(&data) {
                        Ok((m, _)) => {
                            let r = fit(&m, 128, 128);
                            check(&format!("{label} emoji"), &encode_png(&r), &r, &c["emoji"]);
                        }
                        Err(e) => assert_eq!(c["emoji"]["err"], e.to_string(), "{label} emoji"),
                    }
                }
            } else {
                assert!(c["emoji"].is_null(), "{label} emoji");
            }
        }
    }

    fn b64_str(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn the_resolution_guard_refuses_before_decoding() {
        // A PNG header declaring 70000x70000 with no pixel data: refused by the guard, never
        // allocated. `exceedsResolution` divides, so the edge is exact.
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        let ihdr = [0, 1, 0x11, 0x70, 0, 1, 0x11, 0x70, 8, 2, 0, 0, 0];
        png.extend_from_slice(&ihdr);
        let mut crc_in = b"IHDR".to_vec();
        crc_in.extend_from_slice(&ihdr);
        png.extend_from_slice(&goimage::hash::crc32_ieee(&crc_in).to_be_bytes());
        assert_eq!(
            decode(&png, MAX_RES),
            Err(PipelineError::Go(format!(
                "imaging: image resolution 70000x70000 exceeds the maximum allowed {MAX_RES} pixels"
            )))
        );
        assert!(exceeds_resolution(7681, 4320, MAX_RES));
        assert!(!exceeds_resolution(7680, 4320, MAX_RES));
        assert!(!exceeds_resolution(0, 4320, MAX_RES));
    }

    /// A WebP canvas declaring alpha is the last thing `image.Decode`'s registry answers and this
    /// port does not: Go decodes a lossy frame carrying an `ALPH` chunk into an `*image.NYCbCrA`,
    /// and `goimage::image::Image` has no variant for it. Every other magic reaches a decoder.
    #[test]
    fn unported_formats_are_named_for_the_forward() {
        let alpha = {
            let fx = fixture("webp");
            let c = fx["decode"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == "yellow_rose.lossy-with-alpha.webp")
                .expect("the corpus carries a lossy WebP with an alpha chunk");
            b64(c["b64"].as_str().unwrap())
        };
        assert_eq!(
            decode(&alpha, MAX_RES),
            Err(PipelineError::NotPorted("webp"))
        );
        assert_eq!(
            decode_config(&alpha),
            Err(PipelineError::NotPorted("webp")),
            "both halves hand over together, so the decision lands before the write"
        );
        // Every walk is ported, WebP's included, so a WebP upload forwards on its *pixels*
        // alone — `GetImageOrientation` is no longer a second reason.
        for format in ["png", "jpeg", "gif", "bmp", "tiff", "webp"] {
            assert!(
                crate::imaging_orientation::get_image_orientation(Input::Seeker(b""), format)
                    .is_ok(),
                "{format}"
            );
        }
    }
}
