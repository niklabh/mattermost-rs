//! Port of `model/emoji_search.go` (9 lines) **and** `model/user_access_token_search.go` (8) —
//! **both whole**.
//!
//! Three fields between them. They share a module because neither justifies one of its own, and
//! folding them lets a single oracle cover both.
//!
//! **There is nothing unusual here, and that is the finding.** Snake_case tags like most of the
//! tree, no `omitempty`, no pointers, no methods, no constructors, no validation. The oracle says
//! so with evidence — key lists off the struct tags, each type through its reachable states — and
//! is deliberately short rather than padded to look proportionate to two ported types.
//!
//! The one thing worth stating: [`UserAccessTokenSearch`] has a **single** field, so its zero
//! value is `{"term":""}` and not `{}`. Reaching for `omitempty` on a lone field looks harmless
//! and would change the body a client receives.
//!
//! Four divergences, all standing crate-wide entries and none of them new: [D-057] on both the
//! string and the bool, [D-040] on a folded key, [D-071] on a repeated one.

use serde::{Deserialize, Serialize};

/// Port of `model.EmojiSearch` (emoji_search.go:6).
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmojiSearch {
    #[serde(rename = "term")]
    pub term: String,

    /// Whether to match only emoji names beginning with the term, rather than containing it.
    #[serde(rename = "prefix_only")]
    pub prefix_only: bool,
}

/// Port of `model.UserAccessTokenSearch` (user_access_token_search.go:6).
///
/// One field, and no `omitempty` on it — so the zero value is `{"term":""}` rather than `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAccessTokenSearch {
    #[serde(rename = "term")]
    pub term: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    #[test]
    fn the_zero_values_are_full_objects() {
        assert_eq!(
            go_json_marshal(&EmojiSearch::default()).unwrap(),
            r#"{"term":"","prefix_only":false}"#
        );
        // The lone field still emits: not `{}`.
        assert_eq!(
            go_json_marshal(&UserAccessTokenSearch::default()).unwrap(),
            r#"{"term":""}"#
        );
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: EmojiSearch = serde_json::from_str(r#"{"term":"smile"}"#).unwrap();
        assert_eq!(got.term, "smile");
        assert!(!got.prefix_only);
    }

    /// Nothing validates the term, so anything a client sends round-trips.
    #[test]
    fn nothing_is_validated() {
        let search = EmojiSearch {
            term: "\u{1F600} 日本".into(),
            prefix_only: true,
        };
        let back: EmojiSearch = serde_json::from_str(&go_json_marshal(&search).unwrap()).unwrap();
        assert_eq!(search, back);
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/emoji_search.json");
        let decoded: EmojiSearch = serde_json::from_str(raw).unwrap();
        assert!(!decoded.term.is_empty());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/user_access_token_search.json");
        let decoded: UserAccessTokenSearch = serde_json::from_str(raw).unwrap();
        assert!(!decoded.term.is_empty());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_search_requests.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_search_requests.json"
        ))
        .unwrap()
    }

    fn keys(oracle: &Value, section: &str) -> Vec<String> {
        oracle[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();
        assert_eq!(keys(&oracle, "emoji_keys"), ["term", "prefix_only"]);
        assert_eq!(keys(&oracle, "token_keys"), ["term"]);
    }

    #[test]
    fn the_wire_formats_match_go() {
        let oracle = oracle();

        let cases = oracle["emoji_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the emoji corpus changed size");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            let want = case["json"].as_str().unwrap();
            let decoded: EmojiSearch =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "emoji/{name}");
        }

        let cases = oracle["token_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 3, "the token corpus changed size");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            let want = case["json"].as_str().unwrap();
            let decoded: UserAccessTokenSearch =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "token/{name}");
        }
    }

    /// The point of this test is the **count**: four divergences, all of them standing crate-wide
    /// entries, and nothing else. If a future change to these types introduced a new one, the
    /// count moves and this fails rather than the new case quietly joining an exemption list.
    #[test]
    fn the_only_divergences_are_the_standing_ones() {
        let oracle = oracle();
        let cases = oracle["decode"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the decode corpus changed size");

        // [D-057] on the string and on the bool, [D-040] on the folded key, [D-071] on the
        // repeated one.
        const DIVERGENT: [&str; 4] = ["null_string", "null_bool", "folded_key", "duplicate_key"];

        let mut seen = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<EmojiSearch>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if DIVERGENT.contains(&name) {
                seen += 1;
                assert!(go_ok, "{name}: Go used to accept it");
                if name == "folded_key" {
                    // Go folds the key and populates; we ignore it as unknown.
                    assert!(case["prefix_only"].as_bool().unwrap(), "Go folded it");
                    assert!(!got.unwrap().prefix_only, "{name}: expected the divergence");
                } else {
                    assert!(got.is_err(), "{name}: expected the divergence");
                }
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if !go_ok {
                continue;
            }
            let got = got.unwrap();
            assert_eq!(got.term, case["term"].as_str().unwrap(), "{name}");
            assert_eq!(
                got.prefix_only,
                case["prefix_only"].as_bool().unwrap(),
                "{name}"
            );
            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json_after"].as_str().unwrap(),
                "{name}"
            );
        }

        assert_eq!(seen, DIVERGENT.len(), "a divergent case left the corpus");
    }
}
