//! Port of `model/channel_search.go` (channel_search.go:1–27) — **whole file**.
//!
//! One constant and one struct with eighteen fields: thirteen bools, three strings, a `[]string`
//! and two `*int`. No methods. Almost all of it is uniform, so what matters is the three fields
//! that are not.
//!
//! # `Page` and `PerPage` are `*int` **with** `omitempty`, and that is a three-way
//!
//! Every other nillable field in the tree is a pointer *without* `omitempty`
//! ([`crate::channel_member_history::ChannelMemberHistory::leave_time`],
//! [`crate::channel_data::ChannelData::channel`]), where nil is `null` and the key is always
//! present. Here `omitempty` on a pointer tests **nil-ness, not the pointee**:
//!
//! | value | document |
//! |---|---|
//! | nil | the key is **absent** |
//! | pointer to `0` | `"page":0` — not dropped, because the pointer is non-nil |
//! | pointer to `5` | `"page":5` |
//!
//! Three states, three distinct documents. `Option<i64>` with
//! `skip_serializing_if = "Option::is_none"` reproduces all three; a bare `i64` with a zero-skip
//! predicate would collapse the first two and silently drop a client's explicit `page=0`, which
//! is the first page rather than a missing one.
//!
//! # `TeamIds` has no `omitempty`, three lines above the two that do
//!
//! So nil is `null`, empty is `[]`, and the key is always present — the opposite convention to
//! its neighbours in the same struct. `Option<Vec<String>>` carries it.
//!
//! # `int`, not `int64`
//!
//! Same platform-width type `ClusterStats` uses. [D-074] measured it against `int64` over eleven
//! bounds on a 64-bit host and they agreed on all of them; this module cites that rather than
//! repeating the sweep, with two bound probes in the corpus as a spot check.
//!
//! # Nothing validates, and two pairs of fields contradict each other
//!
//! `group_constrained`/`exclude_group_constrained`,
//! `access_control_policy_enforced`/`exclude_access_control_policy_enforced` and
//! `public`/`private` can all be set together. Nothing in the model package resolves that, and
//! grepping `channels/store` for a caller found none — so whatever reconciles them lives further
//! up than this port reaches. Recorded rather than guessed at.

use serde::{Deserialize, Serialize};

use crate::utils::StringArray;

/// Port of `model.ChannelSearchDefaultLimit` (channel_search.go:6).
pub const CHANNEL_SEARCH_DEFAULT_LIMIT: i64 = 50;

/// Port of `model.ChannelSearch` (channel_search.go:8).
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelSearch {
    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "exclude_default_channels")]
    pub exclude_default_channels: bool,

    #[serde(rename = "not_associated_to_group")]
    pub not_associated_to_group: String,

    /// No `omitempty`, unlike [`Self::page`] below it — so nil is `null` and empty is `[]`, and
    /// the key is always present.
    #[serde(rename = "team_ids")]
    pub team_ids: Option<StringArray>,

    #[serde(rename = "group_constrained")]
    pub group_constrained: bool,

    #[serde(rename = "exclude_group_constrained")]
    pub exclude_group_constrained: bool,

    #[serde(rename = "exclude_policy_constrained")]
    pub exclude_policy_constrained: bool,

    #[serde(rename = "public")]
    pub public: bool,

    #[serde(rename = "private")]
    pub private: bool,

    #[serde(rename = "include_deleted")]
    pub include_deleted: bool,

    #[serde(rename = "include_search_by_id")]
    pub include_search_by_id: bool,

    #[serde(rename = "exclude_remote")]
    pub exclude_remote: bool,

    #[serde(rename = "deleted")]
    pub deleted: bool,

    /// `*int` with `omitempty`: absent when `None`, and **present as `0`** when `Some(0)`. See the
    /// module docs — the zero is a real page number, not a missing value.
    #[serde(rename = "page", skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,

    #[serde(rename = "per_page", skip_serializing_if = "Option::is_none")]
    pub per_page: Option<i64>,

    #[serde(rename = "access_control_policy_enforced")]
    pub access_control_policy_enforced: bool,

    #[serde(rename = "exclude_access_control_policy_enforced")]
    pub exclude_access_control_policy_enforced: bool,

    #[serde(rename = "parent_access_control_policy_id")]
    pub parent_access_control_policy_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    /// The three-way, stated as documents.
    #[test]
    fn the_page_pointer_has_three_states() {
        let absent = ChannelSearch::default();
        assert!(!go_json_marshal(&absent).unwrap().contains("\"page\""));

        let zero = ChannelSearch {
            page: Some(0),
            ..Default::default()
        };
        assert!(go_json_marshal(&zero).unwrap().contains(r#""page":0"#));

        let five = ChannelSearch {
            page: Some(5),
            ..Default::default()
        };
        assert!(go_json_marshal(&five).unwrap().contains(r#""page":5"#));

        // ...and the three are distinct values, not merely distinct renderings.
        assert_ne!(absent, zero);
        assert_ne!(zero, five);
    }

    /// The neighbouring field takes the opposite convention.
    #[test]
    fn team_ids_is_always_present_where_page_is_not() {
        let json = go_json_marshal(&ChannelSearch::default()).unwrap();
        assert!(json.contains(r#""team_ids":null"#), "{json}");
        assert!(!json.contains("\"page\""), "{json}");

        let empty = ChannelSearch {
            team_ids: Some(Vec::new()),
            ..Default::default()
        };
        assert!(
            go_json_marshal(&empty)
                .unwrap()
                .contains(r#""team_ids":[]"#)
        );
    }

    /// Sixteen of eighteen keys are unconditional, so the zero value is a large object.
    #[test]
    fn the_zero_value_emits_sixteen_keys() {
        let json = go_json_marshal(&ChannelSearch::default()).unwrap();
        let decoded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 16, "{json}");
        assert!(!decoded.contains_key("page"));
        assert!(!decoded.contains_key("per_page"));
    }

    /// Nothing reconciles the contradictory pairs at this level.
    #[test]
    fn contradictory_flags_round_trip() {
        let search = ChannelSearch {
            public: true,
            private: true,
            group_constrained: true,
            exclude_group_constrained: true,
            ..Default::default()
        };
        let json = go_json_marshal(&search).unwrap();
        let back: ChannelSearch = serde_json::from_str(&json).unwrap();
        assert_eq!(search, back);
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: ChannelSearch = serde_json::from_str(r#"{"term":"town"}"#).unwrap();
        assert_eq!(got.term, "town");
        assert!(got.page.is_none() && got.team_ids.is_none());
    }

    #[test]
    fn the_default_limit_matches_the_constant() {
        assert_eq!(CHANNEL_SEARCH_DEFAULT_LIMIT, 50);
    }
}

/// Serialization parity against `fixtures/channel_search.json` — the reflection-populated oracle,
/// every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/channel_search.json");
        let decoded: ChannelSearch = serde_json::from_str(raw).unwrap();

        // The fixture populates the pointers, so the `skip_serializing_if` branch is not what
        // this asserts.
        assert!(decoded.page.is_some() && decoded.per_page.is_some());
        assert!(decoded.team_ids.as_ref().is_some_and(|v| !v.is_empty()));
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_search.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_search.json"
        ))
        .unwrap()
    }

    #[test]
    fn the_constant_matches_go() {
        let oracle = oracle();
        assert_eq!(
            oracle["constants"]["ChannelSearchDefaultLimit"]
                .as_i64()
                .unwrap(),
            CHANNEL_SEARCH_DEFAULT_LIMIT
        );
    }

    /// Eighteen keys, read off the Go struct tags. A field added upstream fails here rather than
    /// silently going missing from a search request.
    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();
        let theirs: Vec<&str> = oracle["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(theirs.len(), 18);

        // Everything the fully-populated value emits, plus the two `omitempty` pointers which a
        // zero value drops.
        let full = ChannelSearch {
            page: Some(1),
            per_page: Some(1),
            ..Default::default()
        };
        let mut ours: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(
            &go_json_marshal(&full).unwrap(),
        )
        .unwrap()
        .keys()
        .cloned()
        .collect();
        ours.sort();

        let mut want: Vec<String> = theirs.iter().map(|k| (*k).to_string()).collect();
        want.sort();
        assert_eq!(ours, want);
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert_eq!(cases.len(), 13, "the wire corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: ChannelSearch =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert_eq!(
                decoded.team_ids.is_none(),
                case["team_ids_nil"].as_bool().unwrap(),
                "{name}: team_ids nil"
            );
            assert_eq!(
                decoded.page.is_none(),
                case["page_nil"].as_bool().unwrap(),
                "{name}: page nil"
            );
        }
    }

    /// The module's reason to exist: `omitempty` on a pointer drops nil and **keeps** a pointer to
    /// zero. Asserted through the key's presence in the emitted document, not through the value —
    /// a port that emitted `"page":0` for `None` would pass a value comparison and fail this.
    #[test]
    fn the_omitempty_pointers_match_go() {
        let oracle = oracle();
        let cases = oracle["pointer_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the pointer corpus changed size");

        let (mut present, mut absent) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: ChannelSearch =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");

            let emitted: serde_json::Map<String, Value> =
                serde_json::from_str(&go_json_marshal(&decoded).unwrap()).unwrap();
            let page_present = case["page_key_present"].as_bool().unwrap();
            assert_eq!(
                emitted.contains_key("page"),
                page_present,
                "{name}: page key presence"
            );
            assert_eq!(
                emitted.contains_key("per_page"),
                case["per_page_key_present"].as_bool().unwrap(),
                "{name}: per_page key presence"
            );

            assert_eq!(
                decoded.page.is_none(),
                case["page_nil"].as_bool().unwrap(),
                "{name}: page nil"
            );
            assert_eq!(decoded.page, case["page_value"].as_i64(), "{name}: page");

            if page_present {
                present += 1;
            } else {
                absent += 1;
            }
        }

        // The corpus is worthless unless it still contains both.
        assert_eq!((present, absent), (7, 3), "the presence split moved");

        // And the case the whole entry turns on, named so it cannot silently leave the corpus.
        let zero = cases
            .iter()
            .find(|c| c["name"] == "page_zero")
            .expect("page_zero is missing");
        assert!(
            zero["page_key_present"].as_bool().unwrap(),
            "Go stopped emitting a pointer-to-zero — omitempty's rule changed"
        );
        assert_eq!(zero["page_value"].as_i64().unwrap(), 0);
    }

    #[test]
    fn the_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["decode"].as_array().unwrap();
        assert_eq!(cases.len(), 20, "the decode corpus changed size");

        // `null` into a **bool** is [D-057]: Go leaves `false`, we reject the document. `null`
        // into `page` is not — that field is a pointer in Go too, so `Option<i64>` matches it.
        const NULL_SCALAR: &str = "bool_null";
        // `null` as an element of `[]string`: Go stores `""`, we reject. See [D-075].
        const NULL_ELEMENT: &str = "team_ids_null_element";
        // Go folds case against the tag; the tag here already carries the underscores ([D-040]).
        const FOLDED_KEY: &str = "folded_key";

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelSearch>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if name == NULL_SCALAR {
                assert!(go_ok && !case["public"].as_bool().unwrap());
                assert!(got.is_err(), "{name}: expected the [D-057] divergence");
                continue;
            }

            if name == NULL_ELEMENT {
                assert!(go_ok, "{name}: Go used to accept it");
                assert_eq!(
                    case["team_ids_len"].as_u64().unwrap(),
                    1,
                    "Go kept the slot"
                );
                assert!(
                    case["json_after"]
                        .as_str()
                        .unwrap()
                        .contains(r#""team_ids":[""]"#),
                    "Go filled it with the empty string"
                );
                assert!(got.is_err(), "{name}: expected the [D-075] divergence");
                continue;
            }

            if name == FOLDED_KEY {
                assert!(
                    case["exclude_default_channels"].as_bool().unwrap(),
                    "Go folded it"
                );
                let got = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert!(
                    !got.exclude_default_channels,
                    "{name}: expected the [D-040] divergence"
                );
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if !go_ok {
                continue;
            }

            let got = got.unwrap();
            assert_eq!(
                got.page.is_none(),
                case["page_nil"].as_bool().unwrap(),
                "{name}: page nil"
            );
            assert_eq!(got.page, case["page_value"].as_i64(), "{name}: page");
            assert_eq!(
                got.team_ids.is_none(),
                case["team_ids_nil"].as_bool().unwrap(),
                "{name}: team_ids nil"
            );
            assert_eq!(got.term, case["term"].as_str().unwrap(), "{name}");
            assert_eq!(got.public, case["public"].as_bool().unwrap(), "{name}");
            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json_after"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    /// `null` into `page` is **not** a divergence, unlike every other `null`-into-a-scalar in the
    /// crate — the Go field is a pointer, so nil is a representable result and `Option<i64>`
    /// reproduces it. Asserted separately because the exemption list above is otherwise easy to
    /// misread as "all nulls diverge".
    #[test]
    fn a_null_page_is_not_a_divergence() {
        let oracle = oracle();
        let case = oracle["decode"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "page_null")
            .expect("page_null is missing");

        assert!(case["ok"].as_bool().unwrap() && case["page_nil"].as_bool().unwrap());

        let ours: ChannelSearch = serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
        assert!(ours.page.is_none());
        // ...and it re-emits as an absent key, exactly as Go does.
        assert!(!go_json_marshal(&ours).unwrap().contains("\"page\""));
    }
}
