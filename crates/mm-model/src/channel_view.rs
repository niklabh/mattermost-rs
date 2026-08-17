//! Port of `model/channel_view.go` (channel_view.go:1–15) — **whole file**.
//!
//! Two structs, no methods, no constructor: the request and response of the mark-channel-read
//! endpoint.
//!
//! **Nothing here has `omitempty`**, so every key is always present and the zero value of each
//! struct is a full object rather than `{}`. That is the whole wire format, and it leaves two
//! things worth stating.
//!
//! # `last_viewed_at_times` distinguishes nil from empty, and Go sorts its keys
//!
//! It is the first bare `map[string]int64` in the ported tree. Without `omitempty` a nil map
//! reaches the client as `null` and an empty one as `{}`, so it is an [`Option`], and the inner
//! map is a [`BTreeMap`] because Go sorts map keys by byte value when marshalling — a `HashMap`
//! would emit them in an order that is neither sorted nor stable between runs ([D-027]).
//!
//! Byte value, not collation: `{"A":1,"B":4,"a":3,"b":2}` is Go's ordering for those four keys,
//! and `BTreeMap<String, _>` agrees because `String: Ord` is byte-wise too.
//!
//! # Go accepts `null` into all three scalars and into the map's values
//!
//! Measured, and the last of those was the open question: `{"last_viewed_at_times":{"a":null}}`
//! gives Go a map with **`a` present and set to 0**, not a map without `a`. We reject the whole
//! document instead. Same for `null` into `status`, `channel_id` or
//! `collapsed_threads_supported`. All four are [D-057], asserted rather than skipped.
//!
//! Everything else about the two scalars' decoding matches: Go rejects `"true"`, `1` and `0` into
//! the bool, and rejects `1.0`, `1e9`, a quoted number and an out-of-range integer into the map's
//! `int64` — and so does `serde_json`, on all of them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Port of `model.ChannelView` (channel_view.go:6).
///
/// The request body of the mark-channel-read endpoint. **Nothing in this type is validated** —
/// there is no `IsValid` — so `channel_id` need not be an id and both string fields may be empty.
/// An empty `prev_channel_id` is the documented way to say "entering a channel from nowhere".
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelView {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "prev_channel_id")]
    pub prev_channel_id: String,

    #[serde(rename = "collapsed_threads_supported")]
    pub collapsed_threads_supported: bool,
}

/// Port of `model.ChannelViewResponse` (channel_view.go:12).
///
/// `status` is a free-form string rather than an enum — Go declares it `string` with no
/// constants, and the handler writes `"OK"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelViewResponse {
    #[serde(rename = "status")]
    pub status: String,

    /// Channel id to last-viewed timestamp, in epoch milliseconds.
    ///
    /// `Option` because the field has no `omitempty` and Go's nil map serialises as `null` while
    /// an empty one serialises as `{}`; a plain map would collapse the two. `BTreeMap` because Go
    /// sorts map keys when marshalling — see the module docs.
    #[serde(rename = "last_viewed_at_times")]
    pub last_viewed_at_times: Option<BTreeMap<String, i64>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    #[test]
    fn the_zero_values_are_full_objects() {
        assert_eq!(
            go_json_marshal(&ChannelView::default()).unwrap(),
            r#"{"channel_id":"","prev_channel_id":"","collapsed_threads_supported":false}"#
        );
        assert_eq!(
            go_json_marshal(&ChannelViewResponse::default()).unwrap(),
            r#"{"status":"","last_viewed_at_times":null}"#
        );
    }

    /// The distinction the missing `omitempty` exposes.
    #[test]
    fn a_nil_map_and_an_empty_map_differ_on_the_wire() {
        let nil = ChannelViewResponse {
            status: "OK".into(),
            last_viewed_at_times: None,
        };
        let empty = ChannelViewResponse {
            status: "OK".into(),
            last_viewed_at_times: Some(BTreeMap::new()),
        };

        assert_eq!(
            go_json_marshal(&nil).unwrap(),
            r#"{"status":"OK","last_viewed_at_times":null}"#
        );
        assert_eq!(
            go_json_marshal(&empty).unwrap(),
            r#"{"status":"OK","last_viewed_at_times":{}}"#
        );
        assert_ne!(nil, empty);
    }

    /// Byte value, not collation — uppercase sorts before lowercase.
    #[test]
    fn the_map_keys_are_sorted_by_byte_value() {
        let response = ChannelViewResponse {
            status: String::new(),
            last_viewed_at_times: Some(BTreeMap::from([
                ("b".into(), 2),
                ("A".into(), 1),
                ("a".into(), 3),
                ("B".into(), 4),
            ])),
        };
        assert_eq!(
            go_json_marshal(&response).unwrap(),
            r#"{"status":"","last_viewed_at_times":{"A":1,"B":4,"a":3,"b":2}}"#
        );
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: ChannelView = serde_json::from_str(r#"{"channel_id":"c1"}"#).unwrap();
        assert_eq!(got.channel_id, "c1");
        assert!(got.prev_channel_id.is_empty());
        assert!(!got.collapsed_threads_supported);

        let got: ChannelViewResponse = serde_json::from_str(r#"{"status":"OK"}"#).unwrap();
        assert!(got.last_viewed_at_times.is_none());
    }

    /// No `IsValid` anywhere in the file, so nothing here is rejected.
    #[test]
    fn nothing_is_validated() {
        let got: ChannelView = serde_json::from_str(r#"{"channel_id":"nope"}"#).unwrap();
        assert_eq!(got.channel_id, "nope");
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/channel_view.json");
        let decoded: ChannelView = serde_json::from_str(raw).unwrap();
        assert!(!decoded.channel_id.is_empty() && !decoded.prev_channel_id.is_empty());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/channel_view_response.json");
        let decoded: ChannelViewResponse = serde_json::from_str(raw).unwrap();
        assert!(!decoded.status.is_empty());
        assert!(
            decoded
                .last_viewed_at_times
                .as_ref()
                .is_some_and(|m| !m.is_empty())
        );
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_view.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_view.json"
        ))
        .unwrap()
    }

    /// Go accepts `null` into a scalar and leaves the zero value; we reject the document. See
    /// [D-057].
    const NULL_SCALAR: [&str; 2] = ["explicit_nulls", "explicit_null_status"];

    /// Go matches field names case-insensitively and serde does not. See [D-040].
    const UPPERCASE_KEY: &str = "uppercase_key";

    /// A repeated **struct field**: Go takes the last value, serde's derive errors. See [D-071].
    /// Note this does not apply to a repeated key inside the *map* — `duplicate_key` passes,
    /// because a `BTreeMap` overwrites exactly as Go's map does.
    const DUPLICATE_FIELD: &str = "duplicate_status";

    #[test]
    fn the_view_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["view_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the view corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            assert!(case["ok"].as_bool().unwrap(), "{name}: Go failed to decode");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelView>(doc);

            if NULL_SCALAR.contains(&name) {
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            let got = got.unwrap_or_else(|e| panic!("{name}: {e}"));

            if name == UPPERCASE_KEY {
                assert_eq!(
                    case["channel_id"].as_str().unwrap(),
                    "qr6kf7ztp7yifxt4wm5xn51bke"
                );
                assert!(got.channel_id.is_empty(), "{name}: expected the divergence");
                continue;
            }

            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                got.channel_id,
                case["channel_id"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                got.prev_channel_id,
                case["prev_channel_id"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                got.collapsed_threads_supported,
                case["collapsed_threads_supported"].as_bool().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn the_response_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["response_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 15, "the response corpus changed size");

        let mut nil_maps = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            assert!(case["ok"].as_bool().unwrap(), "{name}: Go failed to decode");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelViewResponse>(doc);

            if NULL_SCALAR.contains(&name) {
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            if name == DUPLICATE_FIELD {
                assert_eq!(
                    case["status"].as_str().unwrap(),
                    "second",
                    "Go took the last"
                );
                let err = got.expect_err("expected the documented [D-071] divergence");
                assert!(err.to_string().contains("duplicate field"), "{err}");
                continue;
            }

            let got = got.unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(got.status, case["status"].as_str().unwrap(), "{name}");
            assert_eq!(
                got.last_viewed_at_times.is_none(),
                case["map_nil"].as_bool().unwrap(),
                "{name}: map nil"
            );
            assert_eq!(
                got.last_viewed_at_times.map_or(0, |m| m.len() as u64),
                case["map_len"].as_u64().unwrap(),
                "{name}: map len"
            );

            if case["map_nil"].as_bool().unwrap() {
                nil_maps += 1;
            }
        }

        // The corpus is only worth anything while it still contains both states of the map.
        assert_eq!(nil_maps, 4, "the nil-map documents changed count");
    }

    /// The map's **value** position, which `file.go`'s duration corpus could not reach. Fourteen
    /// shapes; Go and `serde_json` return the same verdict on thirteen.
    #[test]
    fn the_map_value_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["map_value_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 14, "the map-value corpus changed size");

        let (mut accepted, mut rejected) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelViewResponse>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if name == "null" {
                // The measured answer to the open question: Go creates the key and sets it to
                // zero rather than leaving it out. We reject the document. See [D-057].
                assert!(go_ok);
                assert!(
                    case["probe_present"].as_bool().unwrap(),
                    "Go dropped the key"
                );
                assert_eq!(case["probe_value"].as_i64().unwrap(), 0);
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if go_ok {
                accepted += 1;
                let map = got.unwrap().last_viewed_at_times.unwrap();
                assert_eq!(
                    map.get("probe").copied(),
                    Some(case["probe_value"].as_i64().unwrap()),
                    "{name}"
                );
                assert_eq!(map.get("keep").copied(), Some(7), "{name}");
            } else {
                rejected += 1;
                // Go writes a zero into the map for the failing key and keeps the entry decoded
                // before it, then reports the error anyway. A Rust decode is all or nothing, so
                // only the verdict is comparable — but the partial state is worth pinning
                // because a Go handler that ignores the error would act on it.
                assert!(case["probe_present"].as_bool().unwrap(), "{name}");
                assert_eq!(case["probe_value"].as_i64().unwrap(), 0, "{name}");
                assert_eq!(case["keep_value"].as_i64().unwrap(), 7, "{name}");
            }
        }

        assert_eq!(
            (accepted, rejected),
            (5, 8),
            "the accept/reject split moved"
        );
    }

    /// The bool, which is stricter than the shapes a hand-written client tends to send.
    #[test]
    fn the_bool_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["bool_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 7, "the bool corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelView>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if name == "null" {
                assert!(go_ok, "Go used to accept null into a bool");
                assert!(!case["value"].as_bool().unwrap(), "Go left it false");
                assert!(
                    got.is_err(),
                    "{name}: expected the documented [D-057] divergence"
                );
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if go_ok {
                assert_eq!(
                    got.unwrap().collapsed_threads_supported,
                    case["value"].as_bool().unwrap(),
                    "{name}"
                );
            }
        }
    }

    /// Built in Go rather than decoded, so the ordering is Go's own and not an echo of the input
    /// document — which is what makes this a test of [D-027]'s sorting rather than of round trips.
    #[test]
    fn the_map_marshalling_matches_go() {
        let oracle = oracle();
        let cases = oracle["map_marshal_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the marshal corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            // Rebuilt from Go's own output. `serde_json`'s object parse is order-insensitive, so
            // re-emitting it through a BTreeMap tests our ordering rather than preserving Go's.
            let want = case["map_json"].as_str().unwrap();
            let ours: Option<BTreeMap<String, i64>> = serde_json::from_str(want).unwrap();
            assert_eq!(
                ours.is_none(),
                case["nil"].as_bool().unwrap(),
                "{name}: nil"
            );
            assert_eq!(go_json_marshal(&ours).unwrap(), want, "{name}");

            let response = ChannelViewResponse {
                status: "OK".into(),
                last_viewed_at_times: ours,
            };
            assert_eq!(
                go_json_marshal(&response).unwrap(),
                case["in_struct"].as_str().unwrap(),
                "{name}"
            );
        }
    }
}
