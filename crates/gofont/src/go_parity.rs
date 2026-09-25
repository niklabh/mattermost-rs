//! Replays `fixtures/behaviour_avatar.json`'s `font` and `glyphs` sections — Go's freetype on
//! `fonts/nunito-bold.ttf` — against this crate. The avatar section is `mm-app`'s to replay: the
//! composition is Mattermost's code, not this crate's.

use sha2::{Digest, Sha256};

use crate::face::{AlphaMask, Face, Options};
use crate::fixed::Point26_6;
use crate::truetype::Font;

fn oracle() -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/behaviour_avatar.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("the avatar oracle"))
        .expect("the oracle is JSON")
}

/// The font the oracle was generated from, read from the reference tree as Go reads it — the
/// repository does not carry a copy.
fn font_bytes() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../reference/mattermost/server/fonts/nunito-bold.ttf"
    );
    std::fs::read(path).expect("the reference tree's nunito-bold.ttf")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The mask's bytes under `dr`, row by row from the mask's origin — what the generator's
/// `maskBytes` reads. Go's mask is 512 cache entries tall, so a read that runs past this entry's
/// window reads the next entry's rows, which are zero on a fresh face; so are they here.
fn mask_bytes(mask: &AlphaMask, dr: (i64, i64, i64, i64)) -> Vec<u8> {
    let (w, h) = ((dr.2 - dr.0) as usize, (dr.3 - dr.1) as usize);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let i = y * mask.stride + x;
            out.push(mask.pix.get(i).copied().unwrap_or(0));
        }
    }
    out
}

#[test]
fn the_fonts_numbers_are_gos() {
    let oracle = oracle();
    let bytes = font_bytes();
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        oracle["font_sha256"],
        "the reference tree's font is the one the oracle was generated from"
    );
    let font = Font::parse(&bytes).expect("parses");
    let info = &oracle["font"];
    assert_eq!(i64::from(font.units_per_em()), info["units_per_em"]);
    let face = Face::new(
        font,
        &Options {
            size: 64.0,
            ..Options::default()
        },
    )
    .expect("a face");
    let b = face.font().bounds(face.scale());
    assert_eq!(
        serde_json::json!([b.min.x, b.min.y, b.max.x, b.max.y]),
        info["bounds_64"]
    );
    let m = face.metrics();
    assert_eq!(
        serde_json::json!([m.height, m.ascent, m.descent]),
        info["metrics_64"]
    );
    assert_eq!(
        face.kern('A', 'V').expect("kern"),
        info["kern_av_64"].as_i64().expect("an int") as i32
    );
}

/// Every glyph case: bounds, advance, and at each dot the rectangle, advance and mask bytes.
#[test]
fn every_glyph_matches_gos_rasteriser() {
    let oracle = oracle();
    let bytes = font_bytes();
    let mut failures = Vec::new();
    let cases = oracle["glyphs"].as_array().expect("glyph cases");
    for case in cases {
        let size = case["size"].as_f64().expect("size");
        let r = case["rune"]
            .as_str()
            .and_then(|s| s.chars().next())
            .expect("a rune");
        let font = Font::parse(&bytes).expect("parses");
        assert_eq!(
            i64::from(font.index(r)),
            case["index"],
            "{r:?}: the cmap lookup"
        );
        let mut face = Face::new(
            font,
            &Options {
                size,
                ..Options::default()
            },
        )
        .expect("a face");

        let bounds = face
            .glyph_bounds(r)
            .map(|(b, adv)| (serde_json::json!([b.min.x, b.min.y, b.max.x, b.max.y]), adv));
        match (&bounds, case["bounds"].is_null()) {
            (None, true) => {}
            (Some((b, adv)), false) => {
                if *b != case["bounds"] || i64::from(*adv) != case["bounds_advance"] {
                    failures.push(format!(
                        "{size} {r:?}: bounds {b} {adv} vs {} {}",
                        case["bounds"], case["bounds_advance"]
                    ));
                }
            }
            _ => failures.push(format!("{size} {r:?}: bounds presence differs")),
        }
        let adv = face.glyph_advance(r);
        if adv.is_some() != case["advance_ok"].as_bool().expect("bool")
            || i64::from(adv.unwrap_or(0)) != case["advance"]
        {
            failures.push(format!(
                "{size} {r:?}: advance {adv:?} vs {}",
                case["advance"]
            ));
        }

        for g in case["glyph"].as_array().expect("glyph dots") {
            let dot = Point26_6::new(
                g["dot"][0].as_i64().expect("x") as i32,
                g["dot"][1].as_i64().expect("y") as i32,
            );
            let got = face.glyph(dot, r);
            let ok = g["ok"].as_bool().expect("ok");
            match got {
                None if !ok => {}
                Some(glyph) if ok => {
                    let dr = serde_json::json!([glyph.dr.0, glyph.dr.1, glyph.dr.2, glyph.dr.3]);
                    let sha = hex(&Sha256::digest(mask_bytes(&glyph.mask, glyph.dr)));
                    if dr != g["dr"]
                        || i64::from(glyph.advance) != g["advance"]
                        || sha != g["mask_sha256"]
                    {
                        failures.push(format!(
                            "{size} {r:?} at {:?}: dr {dr} vs {}, advance {} vs {}, mask {}",
                            (dot.x, dot.y),
                            g["dr"],
                            glyph.advance,
                            g["advance"],
                            if sha == g["mask_sha256"] {
                                "same"
                            } else {
                                "differs"
                            }
                        ));
                    }
                }
                other => failures.push(format!("{size} {r:?}: ok {} vs {}", other.is_some(), ok)),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} glyph cases differ from Go:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
