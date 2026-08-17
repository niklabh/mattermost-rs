//! Port of `model/file.go` (file.go:1–20) — **whole file**.
//!
//! One constant and two response structs with no methods. Nineteen of the twenty lines are
//! unremarkable and the twentieth is not.
//!
//! # `Expiration` is a `time.Duration`, and that is a bare nanosecond count on the wire
//!
//! ```go
//! type PresignURLResponse struct {
//!     URL        string        `json:"url"`
//!     Expiration time.Duration `json:"expiration"`
//! }
//! ```
//!
//! `time.Duration` has **no `MarshalJSON`**. It is a defined `int64`, and `encoding/json` treats
//! it as one, so `time.Hour` goes out as `3600000000000` — nanoseconds, as a bare number. The
//! type *does* have a `String()` that renders `1h0m0s`, which is what makes this worth measuring:
//! the human-readable form exists, looks like the obvious wire format, and the encoder never
//! reaches for it. `duration_marshal` records both side by side so a port that picks the wrong
//! one fails a test that says why.
//!
//! Neither obvious Rust type is substitutable. `std::time::Duration` serialises as a
//! `{"secs":…,"nanos":…}` object, and `chrono::TimeDelta` has no serde impl at all. So
//! [`PresignURLResponse::expiration`] is a plain `i64`, like every other Go `int64` in this crate
//! — but it is the **only** time field in the crate that is not epoch milliseconds, which is the
//! trap worth carrying forward.
//!
//! The decode side is where a client's input lands, and Go's integer rules are stricter than they
//! look. Measured over 17 values: `1.0` is rejected even though it is exactly representable,
//! `1e9` is rejected for being spelled as a float, `"1h"` and `"3600000000000"` are both rejected
//! for being strings, and an out-of-range integer is rejected while the fields decoded *before*
//! it stay populated. `serde_json` agrees with Go on all sixteen of those; the seventeenth is
//! `null`, which Go accepts as zero and we reject ([D-057]).
//!
//! # Everything else is the nillable-slice shape
//!
//! Neither field of [`FileUploadResponse`] carries `omitempty`, so nil and empty are both on the
//! wire and differ — `null` versus `[]`. Both are `Option`, and `client_ids` is `Option` for that
//! reason alone rather than because Go's slice is a pointer.

use serde::{Deserialize, Serialize};

use crate::file_info::FileInfo;
use crate::utils::StringArray;

/// Port of `model.MaxImageSize` (file.go:9) — 24 megapixels, roughly 36 MB as a raw image.
///
/// Written as the product rather than as `24_385_536` so it reads like Go's
/// `int64(6048 * 4032)`; both factors are pinned by the oracle, so the arithmetic is checked
/// rather than trusted.
pub const MAX_IMAGE_SIZE: i64 = 6048 * 4032;

/// Port of `model.FileUploadResponse` (file.go:12).
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// and a client sending a partial response would otherwise be rejected — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileUploadResponse {
    /// The uploaded files. No `omitempty`, so a nil slice reaches the client as `null` and an
    /// empty one as `[]`.
    ///
    /// Go's element type is `*FileInfo`, so `[null]` is a legal document there and not here —
    /// [D-033].
    #[serde(rename = "file_infos")]
    pub file_infos: Option<Vec<FileInfo>>,

    /// The client-supplied ids the upload was keyed by, echoed back so a client can pair
    /// responses with requests. Same nil-versus-empty distinction as above.
    #[serde(rename = "client_ids")]
    pub client_ids: Option<StringArray>,
}

/// Port of `model.PresignURLResponse` (file.go:17).
///
/// Both fields are non-pointer scalars without `omitempty`, so the zero value is two keys rather
/// than `{}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresignURLResponse {
    #[serde(rename = "url")]
    pub url: String,

    /// **Nanoseconds.** Go's `time.Duration`, which `encoding/json` marshals as the underlying
    /// `int64` — so an hour is `3600000000000`, not `3600000`, `3600` or `"1h"`.
    ///
    /// This is the only time-valued field in the crate that is not epoch milliseconds. Do not
    /// hand it to any of the `utils` millisecond helpers, and do not "correct" it to a
    /// [`std::time::Duration`]: that serialises as an object.
    #[serde(rename = "expiration")]
    pub expiration: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    #[test]
    fn max_image_size_is_the_product_of_its_factors() {
        assert_eq!(MAX_IMAGE_SIZE, 24_385_536);
    }

    #[test]
    fn the_zero_upload_response_is_two_nulls() {
        assert_eq!(
            go_json_marshal(&FileUploadResponse::default()).unwrap(),
            r#"{"file_infos":null,"client_ids":null}"#
        );
    }

    /// Neither field has `omitempty`, so all four combinations are distinguishable.
    #[test]
    fn nil_and_empty_slices_differ_on_the_wire() {
        let empty = FileUploadResponse {
            file_infos: Some(Vec::new()),
            client_ids: Some(Vec::new()),
        };
        assert_eq!(
            go_json_marshal(&empty).unwrap(),
            r#"{"file_infos":[],"client_ids":[]}"#
        );

        let half = FileUploadResponse {
            file_infos: None,
            client_ids: Some(vec!["c-1".into()]),
        };
        assert_eq!(
            go_json_marshal(&half).unwrap(),
            r#"{"file_infos":null,"client_ids":["c-1"]}"#
        );
    }

    #[test]
    fn the_zero_presign_response_is_two_keys() {
        assert_eq!(
            go_json_marshal(&PresignURLResponse::default()).unwrap(),
            r#"{"url":"","expiration":0}"#
        );
    }

    /// The whole point of the module docs, asserted as a value rather than as prose.
    #[test]
    fn an_hour_of_expiration_is_nanoseconds() {
        let hour = PresignURLResponse {
            url: "https://example.com/f".into(),
            expiration: 3_600_000_000_000,
        };
        assert_eq!(
            go_json_marshal(&hour).unwrap(),
            r#"{"url":"https://example.com/f","expiration":3600000000000}"#
        );

        // The three plausible wrong units, none of which Go emits.
        for wrong in [3_600_000i64, 3_600, 1] {
            assert_ne!(hour.expiration, wrong);
        }
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: PresignURLResponse = serde_json::from_str(r#"{"url":"u"}"#).unwrap();
        assert_eq!(got.url, "u");
        assert_eq!(got.expiration, 0);

        let got: FileUploadResponse = serde_json::from_str(r#"{"client_ids":[]}"#).unwrap();
        assert!(got.file_infos.is_none());
        assert_eq!(got.client_ids, Some(Vec::new()));
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/file_upload_response.json");
        let decoded: FileUploadResponse = serde_json::from_str(raw).unwrap();
        assert!(decoded.file_infos.as_ref().is_some_and(|v| !v.is_empty()));
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/presign_url_response.json");
        let decoded: PresignURLResponse = serde_json::from_str(raw).unwrap();
        assert!(!decoded.url.is_empty() && decoded.expiration != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_file.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_file.json")).unwrap()
    }

    #[test]
    fn the_constant_matches_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        assert_eq!(c["MaxImageSize"].as_i64().unwrap(), MAX_IMAGE_SIZE);
        // The factors too, so the expression above is checked rather than the product alone.
        assert_eq!(
            c["MaxImageSizeWidth"].as_i64().unwrap() * c["MaxImageSizeHeight"].as_i64().unwrap(),
            MAX_IMAGE_SIZE
        );
    }

    /// A `time.Duration` marshals as its nanosecond count, and **not** as `Duration.String()`.
    /// The oracle records both; this asserts we emit the first and never the second.
    #[test]
    fn duration_marshals_as_nanoseconds_and_not_as_its_string() {
        let oracle = oracle();
        let cases = oracle["duration_marshal"].as_array().unwrap();
        assert_eq!(cases.len(), 13, "the duration corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let nanos = case["nanoseconds"].as_i64().unwrap();
            let response = PresignURLResponse {
                url: "u".into(),
                expiration: nanos,
            };

            assert_eq!(
                go_json_marshal(&response).unwrap(),
                case["in_struct"].as_str().unwrap(),
                "{name}"
            );
            // The bare value, too — the field is nothing but the integer.
            assert_eq!(nanos.to_string(), case["json"].as_str().unwrap(), "{name}");

            // And the rendering we must never emit. `1h0m0s` is not JSON-numeric, so this is a
            // guard against a future `Serialize` impl, not against the current one.
            let rendered = case["string"].as_str().unwrap();
            assert_ne!(
                go_json_marshal(&response).unwrap(),
                format!(r#"{{"url":"u","expiration":"{rendered}"}}"#),
                "{name}"
            );
        }
    }

    /// The decode side, which is where a client's input lands. Go and `serde_json` agree on every
    /// case but `null`, and the corpus is driven whole so a future serde change that loosened one
    /// of them would surface here.
    #[test]
    fn duration_unmarshal_matches_go() {
        let oracle = oracle();
        let cases = oracle["duration_unmarshal"].as_array().unwrap();
        assert_eq!(cases.len(), 17, "the unmarshal corpus changed size");

        // Go accepts `null` into a scalar and leaves the zero value; serde rejects the document.
        // See [D-057].
        const NULL_SCALAR: &str = "null";

        let (mut accepted, mut rejected) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<PresignURLResponse>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if name == NULL_SCALAR {
                assert!(go_ok, "{name}: Go used to accept it");
                assert_eq!(case["expiration_after"].as_i64().unwrap(), 0);
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if go_ok {
                accepted += 1;
                let got = got.unwrap();
                assert_eq!(
                    got.expiration,
                    case["expiration_after"].as_i64().unwrap(),
                    "{name}"
                );
                assert_eq!(got.url, case["url_after"].as_str().unwrap(), "{name}");
                assert_eq!(
                    go_json_marshal(&got).unwrap(),
                    case["json_after"].as_str().unwrap(),
                    "{name}"
                );
            } else {
                rejected += 1;
                // Go leaves the field decoded *before* the failure populated, and reports the
                // failure anyway. We have no partial value to inspect — a decode is all or
                // nothing — which is why only the accept/reject verdict is compared here.
                assert_eq!(
                    case["url_after"].as_str().unwrap(),
                    "https://example.com/f",
                    "{name}: Go stopped populating earlier fields"
                );
            }
        }

        assert_eq!(
            (accepted, rejected),
            (6, 10),
            "the accept/reject split moved"
        );
    }

    #[test]
    fn the_upload_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["upload_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 11, "the upload corpus changed size");

        let mut nil_elements = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let element_nils: Vec<bool> = case["info_element_nil"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_bool().unwrap())
                .collect();

            if element_nils.contains(&true) {
                nil_elements += 1;
                // Go stores the nil element and re-emits it; we fail the whole document.
                assert!(
                    serde_json::from_str::<FileUploadResponse>(doc).is_err(),
                    "{name}: expected the documented [D-033] decode failure"
                );
                assert!(case["json"].as_str().unwrap().contains("null"));
                continue;
            }

            let decoded: FileUploadResponse =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                decoded.file_infos.is_none(),
                case["infos_nil"].as_bool().unwrap(),
                "{name}: file_infos nil"
            );
            assert_eq!(
                decoded.client_ids.is_none(),
                case["ids_nil"].as_bool().unwrap(),
                "{name}: client_ids nil"
            );
        }

        assert_eq!(nil_elements, 2, "the [D-033] cases changed count");
    }

    #[test]
    fn the_presign_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["presign_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 8, "the presign corpus changed size");

        // Go matches field names case-insensitively, so `{"URL":…}` populates `url` there and is
        // an unknown key here. See [D-040].
        const UPPERCASE_KEY: &str = "uppercase_key";

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let decoded: PresignURLResponse =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            if name == UPPERCASE_KEY {
                assert_eq!(case["url"].as_str().unwrap(), "https://example.com/f");
                assert!(decoded.url.is_empty(), "{name}: expected the divergence");
                continue;
            }

            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(decoded.url, case["url"].as_str().unwrap(), "{name}");
            assert_eq!(
                decoded.expiration,
                case["expiration"].as_i64().unwrap(),
                "{name}"
            );
        }
    }
}
