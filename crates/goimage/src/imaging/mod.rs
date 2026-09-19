//! Port of `github.com/boxes-ltd/imaging` v1.7.5 — the resampling, cropping and orientation
//! transforms Mattermost's `channels/app/imaging` wraps (Fit, FillCenter, GenerateThumbnail,
//! GeneratePreview, GenerateMiniPreviewImage, MakeImageUpright). Every function returns a new
//! `Image::Nrgba` with its origin at (0, 0), as the Go library does.
//!
//! Go runs each row (or column) in its own goroutine through `parallel`; every row is independent
//! of every other, so the sequential loops here produce the same bytes.

pub mod resize;
pub mod scanner;
pub mod tools;
pub mod transform;

pub use resize::{LANCZOS, NEAREST_NEIGHBOR, ResampleFilter, fill, fit, resize};
pub use tools::{Anchor, clone, crop, crop_anchor};
pub use transform::{flip_h, flip_v, rotate90, rotate180, rotate270, transpose, transverse};

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::image::Image;
    use crate::testsupport::{build, describe, fixture};
    use serde_json::Value as Json;

    /// The Mattermost call sites (channels/app/imaging, AGPL — they live in mm-app), replicated
    /// here only to drive the oracle's ops. `None` means Go returned the input image itself.
    fn apply(m: &Image, op: &Json) -> Option<Image> {
        let w = op["w"].as_i64().unwrap_or(0);
        let h = op["h"].as_i64().unwrap_or(0);
        let b = m.bounds();
        Some(match op["op"].as_str().unwrap() {
            "resize" => resize(m, w, h, &LANCZOS),
            "fit" => fit(m, w, h, &LANCZOS),
            "fill" => fill(m, w, h, Anchor::Center, &LANCZOS),
            "thumbnail" => {
                if b.dx() > b.dy() {
                    resize(m, 120, 0, &LANCZOS)
                } else {
                    resize(m, 0, 100, &LANCZOS)
                }
            }
            "preview" => {
                if b.dx() > 1920 {
                    resize(m, 1920, 0, &LANCZOS)
                } else {
                    return None;
                }
            }
            "mini" => resize(m, 16, 16, &LANCZOS),
            "upright" => match op["orientation"].as_i64().unwrap_or(0) {
                2 => flip_h(m),
                3 => rotate180(m),
                4 => flip_v(m),
                5 => transpose(m),
                6 => rotate270(m),
                7 => transverse(m),
                8 => rotate90(m),
                _ => return None,
            },
            other => panic!("unknown op {other}"),
        })
    }

    /// Every case of `behaviour_imaging_resize.json`: resampling, Fit, Fill and the eight
    /// orientations over every source type a decoder produces.
    #[test]
    fn every_resize_case_matches_go() {
        let cases = fixture("resize")["cases"].as_array().unwrap();
        let mut failures = Vec::new();
        let mut last_spec: Option<(&Json, Image)> = None;
        for c in cases {
            let m = match &last_spec {
                Some((spec, m)) if *spec == &c["spec"] => m.clone(),
                _ => {
                    let m = build(&c["spec"]);
                    last_spec = Some((&c["spec"], m.clone()));
                    m
                }
            };
            let out = apply(&m, &c["op"]);
            let ok = match (&out, c["same"].as_bool().unwrap()) {
                (None, true) => true,
                (Some(o), false) => describe(o) == c["output"],
                _ => false,
            };
            if !ok {
                failures.push(format!("{} {}", c["spec"], c["op"]));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ; first: {:?}",
            failures.len(),
            cases.len(),
            &failures[..failures.len().min(10)]
        );
        assert!(cases.len() > 2000);
    }

    /// The scanner's palette is `make([]color.NRGBA, max(256, len(palette)))` (scanner.go:21), so
    /// an index past the palette reads the zero `NRGBA` — transparent black — where `At` would
    /// panic. The PNG decoder never produces such an index; this pins the scanner's own answer.
    #[test]
    fn an_index_past_the_palette_scans_as_transparent_black() {
        let p = crate::image::Paletted {
            pix: crate::image::Pixels {
                pix: vec![0, 1, 5],
                stride: 3,
                rect: crate::image::Rect::new(0, 0, 3, 1),
            },
            palette: vec![
                crate::image::Color::Rgba([1, 2, 3, 255]),
                crate::image::Color::Nrgba([9, 8, 7, 100]),
            ],
        };
        match clone(&Image::Paletted(p)) {
            Image::Nrgba(c) => assert_eq!(c.pix, [1, 2, 3, 255, 9, 8, 7, 100, 0, 0, 0, 0]),
            other => panic!("{other:?}"),
        }
    }
}
