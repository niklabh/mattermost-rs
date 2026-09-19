//! Port of `server/public/model/link_metadata.go`.
//!
//! `LinkMetadata` "stores arbitrary data about a link posted in a message", and roughly half the
//! file manipulates `github.com/dyatlov/go-opengraph` types, which [`crate::opengraph`] ports:
//! [`truncate_open_graph`], [`filter_svg_images`] and their two helpers are pinned against Go's
//! own output in `fixtures/behaviour_opengraph.json`.
//!
//! Two traps in the hashing half, both pinned against Go:
//!
//! * [`generate_link_metadata_hash`] is **FNV-1, not FNV-1a** — and it is the table's primary key;
//! * [`floor_to_nearest_hour`] floors **downward**, so a pre-epoch timestamp rounds away from
//!   zero, not toward it.

use serde::{Deserialize, Serialize};

use crate::go_url;
use crate::opengraph;
use crate::utils::{AppError, go_to_lower};

/// link_metadata.go:22
pub const LINK_METADATA_TYPE_IMAGE: &str = "image";
/// link_metadata.go:23
pub const LINK_METADATA_TYPE_NONE: &str = "none";
/// link_metadata.go:24
pub const LINK_METADATA_TYPE_OPENGRAPH: &str = "opengraph";
/// link_metadata.go:25
pub const LINK_METADATA_MAX_IMAGES: usize = 5;
/// link_metadata.go:26 — "maximum URL length in LinkMetadata table".
pub const LINK_METADATA_MAX_URL_LENGTH: usize = 2048;

/// Port of `model.LinkMetadataType` (link_metadata.go:29). A defined string type, so it accepts
/// anything — [`LinkMetadata`]'s validation is what narrows it.
pub type LinkMetadataType = String;

/// Port of `model.LinkMetadata` (link_metadata.go:33).
///
/// **No `json:` tags at all**, so every wire key is the Go field name — and note `URL`, not `Url`.
/// Third instance of the `wrangler.go` shape after `channel_member_history.go`.
///
/// `Data` is `any` in Go, holding one of `*PostImage`, `*opengraph.OpenGraph` or nil depending on
/// `Type`. It is a `Value` here: modelling *which* concrete type is present is the decision
/// [D-106] records, and it is what `IsValid` needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkMetadata {
    /// "A value computed from the URL and Timestamp for use as a primary key in the database."
    #[serde(rename = "Hash")]
    pub hash: i64,

    #[serde(rename = "URL")]
    pub url: String,

    #[serde(rename = "Timestamp")]
    pub timestamp: i64,

    #[serde(rename = "Type")]
    pub link_type: LinkMetadataType,

    #[serde(rename = "Data")]
    pub data: Option<serde_json::Value>,
}

/// Port of `GenerateLinkMetadataHash` (link_metadata.go:228).
///
/// # This is FNV-**1**, not FNV-1a
///
/// Go's `fnv.New32()` is FNV-1 — *multiply, then XOR*. `fnv.New32a()` is FNV-1a — *XOR, then
/// multiply*. Almost every FNV helper in other ecosystems defaults to 1a, and reaching for it here
/// would produce a plausible-looking hash that is wrong for every input.
///
/// It matters because this value is the `LinkMetadata` table's **primary key**: the wrong variant
/// silently repartitions the table, and every link-preview lookup misses.
///
/// Two more details the signature hides. The timestamp is written **little-endian** as eight bytes
/// *before* the URL's bytes. And the result is a `uint32` widened to `int64`, so it is always
/// non-negative — a port that went through `i32` would produce negative keys for half its inputs.
pub fn generate_link_metadata_hash(url: &str, timestamp: i64) -> i64 {
    const FNV_OFFSET_BASIS_32: u32 = 2_166_136_261;
    const FNV_PRIME_32: u32 = 16_777_619;

    let mut hash = FNV_OFFSET_BASIS_32;

    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            // FNV-1: multiply first, then XOR. Swapping these two lines is FNV-1a.
            hash = hash.wrapping_mul(FNV_PRIME_32);
            hash ^= u32::from(*byte);
        }
    };

    // `binary.Write(hash, binary.LittleEndian, timestamp)` — eight bytes, little-endian.
    update(&timestamp.to_le_bytes());
    update(url.as_bytes());

    // `int64(hash.Sum32())` — a widening of an unsigned value, never a sign extension.
    i64::from(hash)
}

/// Port of `FloorToNearestHour` (link_metadata.go:216) — "takes a timestamp (in milliseconds) and
/// returns it rounded to the previous hour in UTC".
///
/// # It floors, which for a negative input means away from zero
///
/// Go builds a `time.Time`, truncates the minute/second/nanosecond fields and converts back, which
/// is a floor toward negative infinity. Rust's `/` truncates toward **zero**, so the naive
/// `ms / 3_600_000 * 3_600_000` gives `0` for `-1` where Go gives `-3_600_000`. `div_euclid` is
/// the floor. Measured at three pre-epoch inputs.
///
/// The conversion is to **UTC**, so unlike the day-bounds helpers in `utils` ([D-008]) the answer
/// does not depend on the host's zone.
pub fn floor_to_nearest_hour(ms: i64) -> i64 {
    const MS_PER_HOUR: i64 = 60 * 60 * 1000;
    ms.div_euclid(MS_PER_HOUR) * MS_PER_HOUR
}

/// Port of the unexported `isRoundedToNearestHour` (link_metadata.go:223).
pub fn is_rounded_to_nearest_hour(ms: i64) -> bool {
    floor_to_nearest_hour(ms) == ms
}

/// Port of `IsSVGImageURL` (link_metadata.go:113).
///
/// Parses with `url.Parse` — **not** `ParseRequestURI` — so a relative path or a URL containing a
/// space is accepted, and the test is on the parsed `Path` alone. Two consequences worth knowing:
///
/// * a query or fragment ending in `.svg` does **not** make it an SVG, because neither is part of
///   the path;
/// * `Path` is the **decoded** form, so `/a%2Esvg` decodes to `/a.svg` and *is* an SVG.
///
/// The lower-casing is Go's `strings.ToLower` via [`go_to_lower`], not `str::to_lowercase`
/// ([D-029]).
pub fn is_svg_image_url(image_url: &str) -> bool {
    if image_url.is_empty() {
        return false;
    }

    let Ok(parsed) = go_url::go_parse(image_url) else {
        return false;
    };

    // `parsed.Path` is bytes in our port; the suffix test is on the decoded path.
    let path = String::from_utf8_lossy(&parsed.path);
    let path = go_to_lower(&path);

    path.ends_with(".svg") || path.ends_with(".svgz")
}

/// Port of the unexported `truncateText` (link_metadata.go:48): more than 300 **runes** becomes
/// the first 300 and `[...]`. Go's `%.300s` measures its precision in runes, not bytes, so a
/// title of multi-byte characters is cut at the same character here as there.
pub fn truncate_text(original: &str) -> String {
    match original.char_indices().nth(300) {
        Some((cut, _)) => format!("{}[...]", &original[..cut]),
        None => original.to_owned(),
    }
}

/// Port of the unexported `firstNImages` (link_metadata.go:55). A negative limit means
/// [`LINK_METADATA_MAX_IMAGES`]; nil stays nil and a short list is returned as it is.
pub fn first_n_images(
    images: Option<Vec<opengraph::Image>>,
    max_images: i64,
) -> Option<Vec<opengraph::Image>> {
    let max = usize::try_from(max_images).unwrap_or(LINK_METADATA_MAX_IMAGES);
    images.map(|mut images| {
        images.truncate(max);
        images
    })
}

/// Port of `FilterSVGImages` (link_metadata.go:91) — "removes SVG images", by the extension of
/// either URL or by a declared `image/svg+xml` type. An empty or nil list is returned as it is;
/// any other comes back as a new, **non-nil** list, so a list whose every image was an SVG
/// marshals as `[]`, not `null`.
pub fn filter_svg_images(images: Option<Vec<opengraph::Image>>) -> Option<Vec<opengraph::Image>> {
    match images {
        Some(images) if !images.is_empty() => Some(
            images
                .into_iter()
                .filter(|img| {
                    !(is_svg_image_url(&img.url)
                        || is_svg_image_url(&img.secure_url)
                        || img.type_.starts_with("image/svg+xml"))
                })
                .collect(),
        ),
        other => other,
    }
}

/// Port of `TruncateOpenGraph` (link_metadata.go:69) — "ensure OpenGraph metadata doesn't grow
/// too big". Three strings are truncated, the article, book and profile, the determiner, the
/// locales, the audios and the videos are blanked, and the images are cut to five and filtered
/// for SVG. **`type`, `url` and `music` are left alone**, so a `music` object survives into an
/// embed while a `video` does not.
pub fn truncate_open_graph(og: Option<opengraph::OpenGraph>) -> Option<opengraph::OpenGraph> {
    og.map(|mut og| {
        og.title = truncate_text(&og.title);
        og.description = truncate_text(&og.description);
        og.site_name = truncate_text(&og.site_name);
        og.article = None;
        og.book = None;
        og.profile = None;
        og.determiner = String::new();
        og.locale = String::new();
        og.locales_alternate = None;
        og.images = filter_svg_images(first_n_images(
            og.images.take(),
            LINK_METADATA_MAX_IMAGES as i64,
        ));
        og.audios = None;
        og.videos = None;
        og
    })
}

impl LinkMetadata {
    /// Port of `(*LinkMetadata).PreSave` (link_metadata.go:125). Sets the hash and nothing else.
    pub fn pre_save(&mut self) {
        self.hash = generate_link_metadata_hash(&self.url, self.timestamp);
    }

    /// Port of `(*LinkMetadata).IsValid` (link_metadata.go:129), which `LinkMetadataStore.Save`
    /// runs before anything else.
    ///
    /// Go's two `data_type` branches are **type assertions** — `o.Data.(*PostImage)` and
    /// `o.Data.(*opengraph.OpenGraph)` — which a `Value` cannot answer ([D-106]). The one writer
    /// in this port, `saveLinkMetadataToDatabase`, derives `Type` *from* the concrete value it
    /// holds, exactly as Go's does, so the assertion cannot fail there; what is checked here is
    /// the nil half, which the writer can reach. A `none` row with data is the `data_type` error,
    /// as in Go, since that one needs no assertion.
    pub fn is_valid(&self) -> Result<(), Box<AppError>> {
        let error =
            |id: &str, params: Option<std::collections::HashMap<String, serde_json::Value>>| {
                AppError::boxed(
                    "LinkMetadata.IsValid",
                    format!("model.link_metadata.is_valid.{id}.app_error"),
                    params,
                    String::new(),
                    400,
                )
            };
        if self.url.is_empty() {
            return Err(error("url", None));
        }
        if self.url.len() > LINK_METADATA_MAX_URL_LENGTH {
            return Err(error(
                "url_length",
                Some(std::collections::HashMap::from([
                    (
                        "MaxLength".to_owned(),
                        serde_json::json!(LINK_METADATA_MAX_URL_LENGTH),
                    ),
                    ("Length".to_owned(), serde_json::json!(self.url.len())),
                ])),
            ));
        }
        if self.timestamp == 0 || !is_rounded_to_nearest_hour(self.timestamp) {
            return Err(error("timestamp", None));
        }
        match self.link_type.as_str() {
            LINK_METADATA_TYPE_IMAGE | LINK_METADATA_TYPE_OPENGRAPH => {
                if self.data.is_none() {
                    return Err(error("data", None));
                }
            }
            LINK_METADATA_TYPE_NONE => {
                if self.data.is_some() {
                    return Err(error("data_type", None));
                }
            }
            _ => return Err(error("type", None)),
        }
        Ok(())
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_link_metadata.json"
        ))
        .unwrap()
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["LinkMetadataTypeImage"], LINK_METADATA_TYPE_IMAGE);
        assert_eq!(c["LinkMetadataTypeNone"], LINK_METADATA_TYPE_NONE);
        assert_eq!(c["LinkMetadataTypeOpengraph"], LINK_METADATA_TYPE_OPENGRAPH);
        assert_eq!(c["LinkMetadataMaxImages"], LINK_METADATA_MAX_IMAGES);
        assert_eq!(c["LinkMetadataMaxURLLength"], LINK_METADATA_MAX_URL_LENGTH);
    }

    /// No json tags, so PascalCase field names — and `URL`, not `Url`.
    #[test]
    fn wire_keys_are_the_go_field_names() {
        let oracle = oracle();
        let theirs: Vec<&str> = oracle["keys"]["link_metadata"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(theirs, vec!["Hash", "URL", "Timestamp", "Type", "Data"]);

        let ours = serde_json::to_string(&LinkMetadata::default()).unwrap();
        for key in &theirs {
            assert!(ours.contains(&format!("\"{key}\":")), "missing key {key}");
        }
    }

    /// Byte-exact **except** where `Data` holds a struct — see the note below, which is the
    /// limitation [D-106] records.
    #[test]
    fn wire_format_is_byte_exact() {
        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let v: LinkMetadata = serde_json::from_str(expected).unwrap();
            let ours = crate::utils::go_json_marshal(&v).unwrap();

            if name == "type_image" {
                // `Data` is `any` in Go and `serde_json::Value` here, and a `Value::Object` is a
                // BTreeMap — so it sorts keys. Go emits `*PostImage` in the struct's field order,
                // `width height format frame_count`; we emit `format frame_count height width`.
                //
                // The values are identical and any JSON reader sees the same document, so this is
                // a byte-level difference only. It is asserted as such rather than skipped,
                // because the fix is a modelling decision — typing `Data` as an enum over the
                // concrete variants — and that decision is [D-106], not something to make
                // silently here.
                let ours_value: serde_json::Value = serde_json::from_str(&ours).unwrap();
                let theirs_value: serde_json::Value = serde_json::from_str(expected).unwrap();
                assert_eq!(ours_value, theirs_value, "{name}: values must still match");
                assert_ne!(
                    ours, expected,
                    "{name}: if this ever becomes byte-equal, D-106 has been resolved and this \
                     branch should be deleted"
                );
                continue;
            }

            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    /// The FNV-1 corpus. A port that reached for FNV-1a fails every one of these.
    #[test]
    fn hash_matches_go() {
        for case in oracle()["hash"].as_array().unwrap() {
            let url = case["url"].as_str().unwrap();
            let timestamp = case["timestamp"].as_i64().unwrap();
            let expected = case["hash"].as_i64().unwrap();
            assert_eq!(
                generate_link_metadata_hash(url, timestamp),
                expected,
                "hash mismatch for url={url:?} timestamp={timestamp}"
            );
            assert!(
                case["non_negative"].as_bool().unwrap(),
                "Go's hash is a widened uint32 and must never be negative"
            );
        }
    }

    /// Stated on its own: this is FNV-1, and FNV-1a would be a different number.
    #[test]
    fn the_hash_is_fnv_1_not_fnv_1a() {
        // FNV-1a over the same bytes, for contrast.
        fn fnv_1a(url: &str, timestamp: i64) -> i64 {
            let mut hash: u32 = 2_166_136_261;
            let mut update = |bytes: &[u8]| {
                for byte in bytes {
                    hash ^= u32::from(*byte);
                    hash = hash.wrapping_mul(16_777_619);
                }
            };
            update(&timestamp.to_le_bytes());
            update(url.as_bytes());
            i64::from(hash)
        }

        let url = "https://example.com";
        let ts = 1_700_000_000_000i64;
        assert_ne!(
            generate_link_metadata_hash(url, ts),
            fnv_1a(url, ts),
            "if these ever agree the corpus has stopped distinguishing the variants"
        );
    }

    #[test]
    fn floor_to_nearest_hour_matches_go() {
        for case in oracle()["floor_hour"].as_array().unwrap() {
            let input = case["input"].as_i64().unwrap();
            assert_eq!(
                floor_to_nearest_hour(input),
                case["floored"].as_i64().unwrap(),
                "floor mismatch for {input}"
            );
            assert_eq!(
                is_rounded_to_nearest_hour(input),
                case["is_rounded"].as_bool().unwrap(),
                "is_rounded mismatch for {input}"
            );
        }
    }

    /// The floor goes downward, so a pre-epoch millisecond rounds away from zero. Truncating
    /// division would give 0 here.
    #[test]
    fn the_floor_is_downward_not_toward_zero() {
        assert_eq!(floor_to_nearest_hour(-1), -3_600_000);
        assert_eq!(floor_to_nearest_hour(-3_600_001), -7_200_000);
        // What a naive `/` would have produced. Written as a function rather than a literal
        // expression so it is computed rather than const-folded — the contrast is the point.
        fn truncating_floor(ms: i64) -> i64 {
            (ms / 3_600_000) * 3_600_000
        }
        assert_eq!(truncating_floor(-1), 0, "`/` truncates toward zero");
        assert_ne!(floor_to_nearest_hour(-1), truncating_floor(-1));
        // ...and the two agree for non-negative inputs, which is why the bug hides.
        assert_eq!(
            floor_to_nearest_hour(3_600_001),
            truncating_floor(3_600_001)
        );
    }

    #[test]
    fn is_svg_image_url_matches_go() {
        for case in oracle()["is_svg_url"].as_array().unwrap() {
            let url = case["url"].as_str().unwrap();
            assert_eq!(
                is_svg_image_url(url),
                case["is_svg"].as_bool().unwrap(),
                "is_svg mismatch for {url:?}"
            );
        }
    }

    /// The two cases a reading gets wrong: the query is not the path, and the path is decoded.
    #[test]
    fn only_the_decoded_path_decides() {
        assert!(!is_svg_image_url("https://example.com/a.png?x=.svg"));
        assert!(!is_svg_image_url("https://example.com/a.png#.svg"));
        assert!(
            is_svg_image_url("https://example.com/a%2Esvg"),
            "url.Parse decodes Path, so %2E is a dot"
        );
    }

    #[test]
    fn pre_save_matches_go() {
        let case = &oracle()["pre_save"][0];
        let mut m = LinkMetadata {
            url: "https://example.com/page".to_owned(),
            timestamp: 1_700_000_000_000,
            ..Default::default()
        };
        m.pre_save();

        assert_eq!(m.hash, case["hash"].as_i64().unwrap());
        assert_eq!(m.url, case["url"].as_str().unwrap());
        assert_eq!(m.timestamp, case["timestamp"].as_i64().unwrap());
        assert!(case["matches_generate"].as_bool().unwrap());
    }
}

/// [`LinkMetadata::is_valid`] against Go's `IsValid` over `behaviour_link_metadata.json`'s
/// `is_valid` corpus, rebuilt case by case. The two cases whose `Data` is the wrong *Go type*
/// (`type_image_with_wrong_data`, `type_image_with_value_not_pointer`) are the type assertion a
/// `Value` cannot express; they are asserted to be the only cases skipped.
#[cfg(test)]
mod is_valid_parity {
    use super::*;

    fn case(name: &str) -> Option<LinkMetadata> {
        let mut m = LinkMetadata {
            url: "https://example.com/page".to_owned(),
            timestamp: floor_to_nearest_hour(1_700_000_000_000),
            link_type: LINK_METADATA_TYPE_NONE.to_owned(),
            ..Default::default()
        };
        let image = || serde_json::json!({"width": 1, "height": 1, "format": "", "frame_count": 0});
        match name {
            "valid_none" => {}
            "empty_url" => m.url.clear(),
            "url_at_cap" => {
                m.url = format!(
                    "https://e.com/{}",
                    "a".repeat(LINK_METADATA_MAX_URL_LENGTH - "https://e.com/".len())
                );
            }
            "url_over_cap" => {
                m.url = format!("https://e.com/{}", "a".repeat(LINK_METADATA_MAX_URL_LENGTH));
            }
            "zero_timestamp" => m.timestamp = 0,
            "unrounded_timestamp" => m.timestamp = 1_700_000_000_001,
            "type_none_with_data" => m.data = Some(image()),
            "type_image_without_data" => m.link_type = LINK_METADATA_TYPE_IMAGE.to_owned(),
            "type_image_with_post_image" => {
                m.link_type = LINK_METADATA_TYPE_IMAGE.to_owned();
                m.data = Some(image());
            }
            "unknown_type" => m.link_type = "something-else".to_owned(),
            "empty_type" => m.link_type.clear(),
            _ => return None,
        }
        Some(m)
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_link_metadata.json"
        ))
        .unwrap();
        let mut skipped = Vec::new();
        for c in oracle["is_valid"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let Some(m) = case(name) else {
                skipped.push(name.to_owned());
                continue;
            };
            match m.is_valid() {
                Ok(()) => assert_eq!(c["ok"], true, "{name}"),
                Err(err) => {
                    assert_eq!(c["ok"], false, "{name}");
                    assert_eq!(c["id"], err.id.as_str(), "{name}");
                    assert_eq!(c["where"], err.where_.as_str(), "{name}");
                    assert_eq!(c["status"], err.status_code, "{name}");
                }
            }
        }
        assert_eq!(
            skipped,
            vec![
                "type_image_with_wrong_data",
                "type_image_with_value_not_pointer"
            ]
        );
    }
    /// `FilterSVGImages` tests both URLs and the declared type; an all-SVG list becomes `[]`.
    #[test]
    fn filter_svg_images_checks_both_urls_and_the_type() {
        let img = |url: &str, secure: &str, t: &str| opengraph::Image {
            url: url.into(),
            secure_url: secure.into(),
            type_: t.into(),
            ..opengraph::Image::default()
        };
        let kept = filter_svg_images(Some(vec![
            img("a.png", "", ""),
            img("b.png", "https://x/b.svgz", ""),
            img("c.png", "", "image/svg+xml; q=1"),
            img("d.SVG", "", ""),
        ]));
        assert_eq!(kept, Some(vec![img("a.png", "", "")]));
        assert_eq!(
            filter_svg_images(Some(vec![img("x.svg", "", "")])),
            Some(vec![])
        );
        assert_eq!(filter_svg_images(None), None);
    }

    /// Every parse in `fixtures/behaviour_opengraph.json`, read back from Go's JSON as the DB
    /// read path would, must re-marshal to the same bytes — and truncate to Go's truncation.
    #[test]
    fn open_graph_round_trips_and_truncates_like_go() {
        let o: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_opengraph.json"))
                .unwrap();
        let mut n = 0;
        for case in o["opengraph"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            // Go wrote that page's raw invalid bytes as the escape `\ufffd`; read back, they are
            // the character, which Go too would then write as itself. Not a round trip in Go.
            if case["parsed_err"].as_bool().unwrap() || name == "invalid_utf8_raw_absolute" {
                continue;
            }
            let parsed = case["parsed"].as_str().unwrap();
            let og: opengraph::OpenGraph = serde_json::from_str(parsed).unwrap();
            assert_eq!(
                crate::utils::go_json_marshal(&og).unwrap(),
                parsed,
                "{name}"
            );
            assert_eq!(
                crate::utils::go_json_marshal(&truncate_open_graph(Some(og))).unwrap(),
                case["truncated"].as_str().unwrap(),
                "{name}"
            );
            n += 1;
        }
        assert!(n > 450);
        assert_eq!(truncate_open_graph(None), None);
    }
}
