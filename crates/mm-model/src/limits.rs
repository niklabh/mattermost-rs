//! Port of `model/limits.go` (limits.go:1–14) — **whole file**.
//!
//! One struct, seven `int64` fields, no methods, no pointers, no `omitempty`. The types are the
//! least interesting thing about it.
//!
//! # Every key is camelCase
//!
//! ```go
//! MaxUsersLimit          int64 `json:"maxUsersLimit"`
//! LastAccessiblePostTime int64 `json:"lastAccessiblePostTime"`
//! ```
//!
//! That is the **third** naming convention in the ported tree: snake_case everywhere else that has
//! tags, tagless PascalCase in [`crate::wrangler`] and
//! [`crate::channel_member_history`], and camelCase here. It is the sort of thing a translator
//! normalises to `max_users_limit` without noticing, after sixty files of snake_case in a row —
//! and a mis-tagged field would still round-trip cleanly through its own serializer, so only a
//! comparison against Go's key list catches it. `the_wire_keys_match_go` is that comparison.
//!
//! Measured: `max_users_limit` populates **nothing** in Go either. It is not a spelling Go's
//! case-folding fallback reaches, because that folds case and not punctuation ([D-040]).
//!
//! # Zero is a sentinel on four of the seven fields
//!
//! Go's own comments say so: `postHistoryLimit` is "the actual message history limit value (0 if
//! no limits)" and `lastAccessiblePostTime` is "timestamp of the last accessible post (0 if no
//! limits reached)". `maxUsersLimit` and `singleChannelGuestLimit` read the same way.
//!
//! Nothing carries `omitempty`, so every key is transmitted and the zero survives the wire intact
//! — which is what makes the sentinel usable. A port that added `skip_serializing_if` for
//! tidiness would turn "unlimited" into "unspecified".
//!
//! # Nothing is validated
//!
//! A hard limit below the soft limit, an active count above both, a negative limit — all
//! representable and all round-trip. These are computed figures on their way out to an admin
//! console.

use serde::{Deserialize, Serialize};

/// Port of `model.ServerLimits` (limits.go:6).
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerLimits {
    /// Soft limit for the maximum number of users. **Zero means no limit.**
    #[serde(rename = "maxUsersLimit")]
    pub max_users_limit: i64,

    /// Hard limit for the maximum number of *active* users.
    #[serde(rename = "maxUsersHardLimit")]
    pub max_users_hard_limit: i64,

    /// The actual number of active users, where active means not deleted.
    #[serde(rename = "activeUserCount")]
    pub active_user_count: i64,

    /// Guests who belong to exactly one channel.
    #[serde(rename = "singleChannelGuestCount")]
    pub single_channel_guest_count: i64,

    /// Equals the licensed seats, one for one.
    #[serde(rename = "singleChannelGuestLimit")]
    pub single_channel_guest_limit: i64,

    /// The message-history limit in force. **Zero means no limit** — Go's comment says so, and
    /// the field has no `omitempty`, so the zero reaches the client.
    #[serde(rename = "postHistoryLimit")]
    pub post_history_limit: i64,

    /// Epoch milliseconds of the last accessible post. **Zero means no limit has been reached.**
    #[serde(rename = "lastAccessiblePostTime")]
    pub last_accessible_post_time: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    /// The whole content of the port, stated as a document.
    #[test]
    fn every_key_is_camel_case() {
        let json = go_json_marshal(&ServerLimits::default()).unwrap();
        assert_eq!(
            json,
            r#"{"maxUsersLimit":0,"maxUsersHardLimit":0,"activeUserCount":0,"singleChannelGuestCount":0,"singleChannelGuestLimit":0,"postHistoryLimit":0,"lastAccessiblePostTime":0}"#
        );

        // The spelling habit would have produced, which is not a key on either side.
        assert!(!json.contains("max_users_limit"), "{json}");
    }

    /// No `omitempty`, so the sentinel zero is transmitted rather than dropped.
    #[test]
    fn the_all_zero_document_still_carries_seven_keys() {
        let decoded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&go_json_marshal(&ServerLimits::default()).unwrap()).unwrap();
        assert_eq!(decoded.len(), 7);
        assert!(decoded.values().all(|v| v == &serde_json::json!(0)));
    }

    #[test]
    fn nothing_is_validated() {
        // A hard limit below the soft one, and an active count above both.
        let limits = ServerLimits {
            max_users_limit: 100,
            max_users_hard_limit: 10,
            active_user_count: 500,
            ..Default::default()
        };
        let back: ServerLimits = serde_json::from_str(&go_json_marshal(&limits).unwrap()).unwrap();
        assert_eq!(limits, back);
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: ServerLimits = serde_json::from_str(r#"{"activeUserCount":5}"#).unwrap();
        assert_eq!(got.active_user_count, 5);
        assert_eq!(got.max_users_limit, 0);
    }

    /// The snake_case spelling is an unknown key here, so it silently leaves the field zero —
    /// which is why the key list is asserted against Go's rather than trusted.
    #[test]
    fn the_snake_case_spelling_is_an_unknown_key() {
        let got: ServerLimits = serde_json::from_str(r#"{"max_users_limit":42}"#).unwrap();
        assert_eq!(got.max_users_limit, 0);
    }
}

/// Serialization parity against `fixtures/server_limits.json` — the reflection-populated oracle,
/// every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/server_limits.json");
        let decoded: ServerLimits = serde_json::from_str(raw).unwrap();
        assert!(decoded.max_users_limit != 0 && decoded.last_accessible_post_time != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_limits.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_limits.json")).unwrap()
    }

    /// The test this module exists for. The keys are read off the Go struct tags by reflection,
    /// and compared in **order**, because order is the emission order.
    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();
        let theirs: Vec<&str> = oracle["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();

        assert_eq!(
            theirs,
            [
                "maxUsersLimit",
                "maxUsersHardLimit",
                "activeUserCount",
                "singleChannelGuestCount",
                "singleChannelGuestLimit",
                "postHistoryLimit",
                "lastAccessiblePostTime",
            ]
        );

        // Not merely a restatement of the fixture: the same list, off our own serializer.
        let emitted = go_json_marshal(&ServerLimits::default()).unwrap();
        for key in &theirs {
            assert!(
                emitted.contains(&format!(r#""{key}":"#)),
                "we do not emit {key}: {emitted}"
            );
        }
        let decoded: serde_json::Map<String, Value> = serde_json::from_str(&emitted).unwrap();
        assert_eq!(decoded.len(), theirs.len(), "we emit a key Go does not");
    }

    /// Every field driven on its own, so a swapped tag shows up as the wrong key carrying the
    /// value rather than as a document that merely still parses.
    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert_eq!(cases.len(), 14, "the wire corpus changed size");

        let mut singles = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: ServerLimits =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");

            if name.starts_with("only_") {
                singles += 1;
                // Exactly one non-zero value, so a tag swap between two fields cannot pass.
                let values: serde_json::Map<String, Value> = serde_json::from_str(want).unwrap();
                let non_zero: Vec<&String> = values
                    .iter()
                    .filter(|(_, v)| v.as_i64() != Some(0))
                    .map(|(k, _)| k)
                    .collect();
                assert_eq!(non_zero.len(), 1, "{name}: {want}");
            }
        }

        assert_eq!(singles, 7, "one single-field probe per field");
    }

    /// [D-040]'s reach is **wider** for a camelCase tag than for a snake_case one, and the reason
    /// is worth stating: the Go field name `MaxUsersLimit` is itself a case-variant of the tag
    /// `maxUsersLimit`, so Go accepts both. A snake_case tag admits no PascalCase spelling at all.
    ///
    /// The row that matters most is `snake_case`, which **neither** side accepts — so a
    /// mis-tagged Rust field is invisible to a round trip and visible only here.
    #[test]
    fn the_key_casing_matches_go() {
        let oracle = oracle();
        let cases = oracle["key_casing"].as_array().unwrap();
        assert_eq!(cases.len(), 7, "the casing corpus changed size");

        let (mut divergent, mut agreed) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let decoded: ServerLimits =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            let ours = decoded.max_users_limit != 0;
            let theirs = case["populated"].as_bool().unwrap();

            if name == "declared_tag" {
                assert!(ours && theirs, "the declared tag must work on both sides");
                agreed += 1;
            } else if theirs {
                assert!(
                    !ours,
                    "{name}: we accepted a spelling [D-040] says we reject"
                );
                divergent += 1;
            } else {
                assert!(!ours, "{name}");
                agreed += 1;
            }
        }

        assert_eq!(divergent, 4, "the [D-040] spellings changed count");
        assert_eq!(agreed, 3, "the tag, plus snake_case and kebab_case");

        // Named explicitly, because it is the one a port would trip over rather than a
        // theoretical case-variant.
        let snake = cases
            .iter()
            .find(|c| c["name"] == "snake_case")
            .expect("snake_case is missing");
        assert!(
            !snake["populated"].as_bool().unwrap(),
            "Go started accepting max_users_limit — the case fold now covers punctuation"
        );
    }

    #[test]
    fn the_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["decode"].as_array().unwrap();
        assert_eq!(cases.len(), 13, "the decode corpus changed size");

        // Go accepts `null` into a scalar and leaves the zero value; we reject ([D-057]). A
        // repeated struct field takes the last value in Go and errors here ([D-071]).
        const DIVERGENT: [&str; 2] = ["null_int", "duplicate_key"];

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ServerLimits>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if DIVERGENT.contains(&name) {
                assert!(go_ok, "{name}: Go used to accept it");
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            if !go_ok {
                continue;
            }

            let got = got.unwrap();
            assert_eq!(
                got.active_user_count,
                case["active_user_count"].as_i64().unwrap(),
                "{name}"
            );
            assert_eq!(
                got.max_users_limit,
                case["max_users_limit"].as_i64().unwrap(),
                "{name}"
            );
            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json_after"].as_str().unwrap(),
                "{name}"
            );
        }
    }
}
