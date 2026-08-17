//! Port of `model/analytics_row.go` (analytics_row.go:1–11) — **whole file**.
//!
//! ```go
//! type AnalyticsRow struct {
//!     Name  string  `json:"name"`
//!     Value float64 `json:"value"`
//! }
//!
//! type AnalyticsRows []*AnalyticsRow
//! ```
//!
//! Eleven lines, no methods — and the **first `float64` on the wire in the crate**, which is the
//! whole content of the port.
//!
//! # serde_json's float rendering is not Go's, and the divergence is on ordinary values
//!
//! `Value` is a count or an average, so the values this type carries in practice are small
//! integers. Go writes `1`; serde_json writes `1.0`. Measured over 29 values, the two disagree on
//! **12** — every integral value, both sides of the `1e-6` threshold, and the largest float below
//! `1e21`. `%v` disagrees on a different 10.
//!
//! So [`AnalyticsRow::value`] is serialized through [`utils::go_json_format_float`], which
//! reproduces `encoding/json`'s own encoder. Note this is a *third* rendering: the crate already
//! has [`utils::go_format_float`] for `%v`, and that one is wrong here — `%v` writes `1234567` as
//! `1.234567e+06`. The oracle records all three side by side so picking the wrong one fails a
//! test rather than a review.
//!
//! # NaN and the infinities abort the whole document
//!
//! `json.Marshal` returns `json: unsupported value: NaN` and emits **nothing** — not `null`, not
//! `0`. Measured at all three levels: the bare value, the row, and a slice where one good row
//! precedes the bad one. The good row is lost too.
//!
//! That makes serialization **fallible** for this type in a way no other ported type is, and it
//! is why [`AnalyticsRow`] has a hand-written [`Serialize`] rather than a derive: the derive
//! would hand serde_json an `f64` and get `null` from some formats and a panic-free wrong answer
//! from others. Here it is an error, as in Go.

use serde::ser::{Error as SerError, SerializeStruct};
use serde::{Deserialize, Serialize, Serializer};

use crate::utils::go_json_format_float;

/// Port of `model.AnalyticsRow` (analytics_row.go:6).
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043]. `Serialize` is hand-written; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AnalyticsRow {
    #[serde(rename = "name")]
    pub name: String,

    /// A count or an average. Rendered by [`go_json_format_float`], **not** by serde_json's
    /// default `f64` encoding, which writes `1.0` where Go writes `1`.
    #[serde(rename = "value")]
    pub value: f64,
}

impl Serialize for AnalyticsRow {
    /// Hand-written for one reason: `value` must go out with Go's float rendering, and a
    /// `NaN`/`Inf` must **fail** rather than degrade.
    ///
    /// The number is emitted through [`serde_json::value::RawValue`] because there is no
    /// serializer method for "a numeric token I have already formatted" — `serialize_f64` would
    /// hand the value back to serde_json's own encoder, which is the thing being replaced.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let rendered = go_json_format_float(self.value).ok_or_else(|| {
            // Go's exact text, including its `+Inf`/`-Inf` spellings — which come from `%v` and
            // are not JSON.
            S::Error::custom(format!(
                "json: unsupported value: {}",
                crate::utils::go_format_float(self.value)
            ))
        })?;

        let number = serde_json::value::RawValue::from_string(rendered)
            .map_err(|e| S::Error::custom(format!("json: unsupported value: {e}")))?;

        let mut s = serializer.serialize_struct("AnalyticsRow", 2)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("value", &number)?;
        s.end()
    }
}

/// Port of `model.AnalyticsRows` (analytics_row.go:11) — Go's `[]*AnalyticsRow`.
///
/// A `#[serde(transparent)]` newtype, like [`crate::channel_list::ChannelList`]: the JSON is a
/// bare array with no wrapping object. Go's element type is a pointer, so `[null]` is a legal
/// document there and not here — [D-033].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnalyticsRows(pub Vec<AnalyticsRow>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    fn row(name: &str, value: f64) -> AnalyticsRow {
        AnalyticsRow {
            name: name.into(),
            value,
        }
    }

    /// The divergence that matters in practice: an analytics count is an integer.
    #[test]
    fn an_integral_value_has_no_decimal_point() {
        assert_eq!(
            go_json_marshal(&row("total_posts", 1234.0)).unwrap(),
            r#"{"name":"total_posts","value":1234}"#
        );

        // ...which is exactly what a derived Serialize would have got wrong.
        assert_eq!(serde_json::to_string(&1234.0_f64).unwrap(), "1234.0");
    }

    #[test]
    fn a_fractional_value_keeps_its_digits() {
        assert_eq!(
            go_json_marshal(&row("avg", 12.5)).unwrap(),
            r#"{"name":"avg","value":12.5}"#
        );
        assert_eq!(
            go_json_marshal(&row("third", 1.0 / 3.0)).unwrap(),
            r#"{"name":"third","value":0.3333333333333333}"#
        );
    }

    /// Positional below `1e-6`, exponent at and under it — and the negative exponent loses a
    /// leading zero where the positive one keeps it.
    #[test]
    fn the_thresholds_are_1e_minus_6_and_1e21() {
        for (value, want) in [
            (1e-6, "0.000001"),
            (9.99999e-7, "9.99999e-7"),
            (1e-7, "1e-7"),
            (1e-10, "1e-10"),
            (9.999999999999999e20, "999999999999999900000"),
            (1e21, "1e+21"),
            (1e22, "1e+22"),
        ] {
            assert_eq!(
                go_json_marshal(&row("v", value)).unwrap(),
                format!(r#"{{"name":"v","value":{want}}}"#),
                "{value:e}"
            );
        }
    }

    #[test]
    fn negative_zero_keeps_its_sign() {
        assert_eq!(
            go_json_marshal(&row("z", -0.0)).unwrap(),
            r#"{"name":"z","value":-0}"#
        );
    }

    /// Not `null`, not `0` — an error, and it takes the whole document with it.
    #[test]
    fn nan_and_the_infinities_fail_the_serialization() {
        for (value, text) in [
            (f64::NAN, "json: unsupported value: NaN"),
            (f64::INFINITY, "json: unsupported value: +Inf"),
            (f64::NEG_INFINITY, "json: unsupported value: -Inf"),
        ] {
            let err = go_json_marshal(&row("bad", value)).unwrap_err();
            assert!(err.to_string().contains(text), "{value}: {err}");
        }
    }

    /// One bad row loses every good one alongside it, as in Go.
    #[test]
    fn a_bad_row_aborts_the_whole_slice() {
        let rows = AnalyticsRows(vec![row("good", 1.0), row("bad", f64::NAN)]);
        let err = go_json_marshal(&rows).unwrap_err();
        assert!(err.to_string().contains("NaN"), "{err}");
    }

    #[test]
    fn the_rows_newtype_is_a_bare_array() {
        let rows = AnalyticsRows(vec![row("a", 1.0), row("b", 2.5)]);
        assert_eq!(
            go_json_marshal(&rows).unwrap(),
            r#"[{"name":"a","value":1},{"name":"b","value":2.5}]"#
        );
        assert_eq!(go_json_marshal(&AnalyticsRows::default()).unwrap(), "[]");
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: AnalyticsRow = serde_json::from_str(r#"{"name":"n"}"#).unwrap();
        assert_eq!(got.name, "n");
        assert_eq!(got.value, 0.0);
    }
}

/// Serialization parity against `fixtures/analytics_row.json` — the reflection-populated oracle,
/// every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/analytics_row.json");
        let decoded: AnalyticsRow = serde_json::from_str(raw).unwrap();
        assert!(!decoded.name.is_empty() && decoded.value != 0.0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_analytics_row.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::{go_format_float, go_json_marshal};
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_analytics_row.json"
        ))
        .unwrap()
    }

    /// The float is reconstructed from its **bits**, not from Go's decimal rendering — parsing
    /// the rendering back would test the parser and could not distinguish two floats that print
    /// the same.
    fn float_of(case: &Value, key: &str) -> f64 {
        f64::from_bits(case[key].as_u64().unwrap())
    }

    /// The load-bearing test: `encoding/json`'s float rendering, and the two renderings it is
    /// **not**.
    #[test]
    fn the_float_rendering_matches_go() {
        let oracle = oracle();
        let cases = oracle["float_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 29, "the float corpus changed size");

        let (mut serde_differs, mut fmt_v_differs) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let value = float_of(case, "bits");
            let want = case["json"].as_str().unwrap();

            assert_eq!(
                go_json_format_float(value).unwrap_or_else(|| panic!("{name}: unexpected None")),
                want,
                "{name}"
            );

            // ...and through the struct, which is what a client receives.
            let row = AnalyticsRow {
                name: name.into(),
                value,
            };
            assert_eq!(
                go_json_marshal(&row).unwrap(),
                case["in_row"].as_str().unwrap(),
                "{name}"
            );

            // The two wrong answers, counted rather than merely described. `%v` is the crate's
            // other float helper and is a plausible mistake; serde_json's default is the other.
            assert_eq!(
                go_format_float(value),
                case["fmt_v"].as_str().unwrap(),
                "{name}: %v"
            );
            if case["fmt_v"].as_str() != Some(want) {
                fmt_v_differs += 1;
            }
            if serde_json::to_string(&value).unwrap() != want {
                serde_differs += 1;
            }
        }

        // If either of these hits zero the corpus has stopped exercising the difference the
        // module exists for.
        assert_eq!(serde_differs, 12, "serde_json's disagreement changed");
        assert_eq!(fmt_v_differs, 10, "%v's disagreement changed");
    }

    /// A JSON number decodes into a `float64` with none of the integer rules that govern an
    /// `int64` field — `1.5`, `1e9` and an integer past `2^53` are all accepted and rounded.
    #[test]
    fn the_float_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["float_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 20, "the decode corpus changed size");

        // Go accepts `null` into a scalar and leaves the zero value; we reject. See [D-057].
        const NULL_SCALAR: &str = "null";

        let (mut accepted, mut rejected) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<AnalyticsRow>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if name == NULL_SCALAR {
                assert!(go_ok, "Go used to accept null into a float");
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if go_ok {
                accepted += 1;
                let got = got.unwrap();
                // Compared as bits: `-0` and `0` are equal under `==` and are different values.
                assert_eq!(
                    got.value.to_bits(),
                    case["value_bits"].as_u64().unwrap(),
                    "{name}: value"
                );
                assert_eq!(got.name, case["name_after"].as_str().unwrap(), "{name}");
            } else {
                rejected += 1;
            }
        }

        assert_eq!(
            (accepted, rejected),
            (12, 7),
            "the accept/reject split moved"
        );
    }

    #[test]
    fn the_row_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["row_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 7, "the row corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: AnalyticsRow =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert_eq!(
                decoded.value.to_bits(),
                case["value_bits"].as_u64().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn the_rows_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["rows_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the rows corpus changed size");

        let mut nil_elements = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let element_nils: Vec<bool> = case["element_nil"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_bool().unwrap())
                .collect();

            if element_nils.contains(&true) {
                nil_elements += 1;
                // Go stores the nil element and re-emits it; we fail the whole document.
                assert!(
                    serde_json::from_str::<AnalyticsRows>(want).is_err(),
                    "{name}: expected the documented [D-033] decode failure"
                );
                assert!(want.contains("null"));
                continue;
            }

            if case["nil"].as_bool().unwrap() {
                // Go's nil slice is `null`, which our `Vec` newtype cannot represent — it has no
                // `Option`, because `AnalyticsRows` is a top-level response body and Go's own
                // producer always appends onto a fresh slice.
                assert_eq!(want, "null", "{name}");
                continue;
            }

            let decoded: AnalyticsRows =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert_eq!(
                decoded.0.len() as u64,
                case["len"].as_u64().unwrap(),
                "{name}"
            );
        }

        assert_eq!(nil_elements, 2, "the [D-033] cases changed count");
    }

    /// The three values Go refuses. Recorded at all three levels, because the failure propagates:
    /// a slice holding one good row and one bad one produces **no output**.
    #[test]
    fn the_unsupported_values_match_go() {
        let oracle = oracle();
        let cases = oracle["unsupported"].as_array().unwrap();
        assert_eq!(cases.len(), 3);

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            assert!(
                !case["ok"].as_bool().unwrap(),
                "{name}: Go used to accept it"
            );
            assert_eq!(
                case["output"].as_str().unwrap(),
                "",
                "{name}: Go emitted bytes"
            );

            let value = float_of(case, "bits");
            assert!(go_json_format_float(value).is_none(), "{name}");

            // The error text, reproduced including Go's `%v` spelling of the value.
            let want = case["err"].as_str().unwrap();
            let row = AnalyticsRow {
                name: "n".into(),
                value,
            };
            let err = go_json_marshal(&row).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: {err} does not contain {want}");
            assert_eq!(case["row_err"].as_str().unwrap(), want, "{name}");

            // And the slice, where a good row is lost alongside the bad one.
            assert_eq!(case["slice_err"].as_str().unwrap(), want, "{name}");
            let rows = AnalyticsRows(vec![
                AnalyticsRow {
                    name: "good".into(),
                    value: 1.0,
                },
                row,
            ]);
            assert!(go_json_marshal(&rows).is_err(), "{name}");
        }
    }
}
