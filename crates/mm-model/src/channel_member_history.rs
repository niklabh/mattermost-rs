//! Port of `model/channel_member_history.go` (11 lines) **and**
//! `model/channel_member_history_result.go` (18 lines) — **both whole**.
//!
//! Two structs, no methods, no constructors. They are paired in one module because they are the
//! same four fields, the second adding four more, and splitting them would duplicate the entire
//! oracle for eleven lines of Go.
//!
//! # Not one `json:` tag between them
//!
//! So every wire key is the Go **field name verbatim** — `ChannelId`, `JoinTime`, `IsBot` —
//! capitalisation included. Second instance of this shape in the tree after
//! [`crate::wrangler::WranglerPostList`], and the whole content of the port: writing these as
//! snake_case out of habit is the failure mode, and the key lists are read off the Go struct tags
//! by the oracle rather than transcribed here.
//!
//! # `UserEmail` has a `db:"Email"` tag and no json tag
//!
//! `encoding/json` does not look at `db`, so the wire key is `UserEmail` while the column is
//! `Email`. The only tag visible on the field is the wrong one to copy.
//!
//! # `LeaveTime` is the one nillable field, and the distinction is load-bearing
//!
//! `*int64` with no `omitempty`, so the key is always present and nil is `null`. It is what
//! separates a member still in the channel from one who has left — and a non-nil pointer to `0`
//! is a third state, which `Option<i64>` carries and a bare `i64` would flatten into "still
//! present".
//!
//! # The casing exposure is total here
//!
//! Go matches the declared name first and falls back to a **case-insensitive** match, so
//! `channelid`, `CHANNELID` and `cHaNnElId` all populate `ChannelId` there and none of them does
//! here. That is [D-040], and this is the type where it is widest — every key is affected, not
//! one. Measured: the fallback is case-insensitive but **not** punctuation-insensitive, so
//! `channel_id` and `channel-id` are unknown keys in Go too.

use serde::{Deserialize, Serialize};

/// Port of `model.ChannelMemberHistory` (channel_member_history.go:6).
///
/// The stored join/leave row. Every wire key is the Go field name; see the module docs.
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelMemberHistory {
    #[serde(rename = "ChannelId")]
    pub channel_id: String,

    #[serde(rename = "UserId")]
    pub user_id: String,

    #[serde(rename = "JoinTime")]
    pub join_time: i64,

    /// Epoch milliseconds, or `None` while the member is still in the channel. No `omitempty`, so
    /// the key is always on the wire and nil is `null`.
    #[serde(rename = "LeaveTime")]
    pub leave_time: Option<i64>,
}

/// Port of `model.ChannelMemberHistoryResult` (channel_member_history_result.go:6).
///
/// [`ChannelMemberHistory`] plus four fields the store fills in by joining on `Users`. Not a
/// superset type in Go — it redeclares the first four rather than embedding — so this is a
/// separate struct here too, with no `Deref` between them.
///
/// Go's comment above the joined group says "these two fields" and there are **four**. Left as
/// upstream wrote it; it reads like the group grew without the comment following.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelMemberHistoryResult {
    #[serde(rename = "ChannelId")]
    pub channel_id: String,

    #[serde(rename = "UserId")]
    pub user_id: String,

    #[serde(rename = "JoinTime")]
    pub join_time: i64,

    #[serde(rename = "LeaveTime")]
    pub leave_time: Option<i64>,

    /// **`UserEmail` on the wire, `Email` in the database.** Go tags this `db:"Email"` and gives
    /// it no json tag, so the serde name is the field name and the `db` tag is not ours to copy.
    #[serde(rename = "UserEmail")]
    pub user_email: String,

    #[serde(rename = "Username")]
    pub username: String,

    #[serde(rename = "IsBot")]
    pub is_bot: bool,

    #[serde(rename = "UserDeleteAt")]
    pub user_delete_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    #[test]
    fn the_wire_keys_are_the_go_field_names() {
        let history = ChannelMemberHistory {
            channel_id: "c1".into(),
            user_id: "u1".into(),
            join_time: 100,
            leave_time: Some(200),
        };
        assert_eq!(
            go_json_marshal(&history).unwrap(),
            r#"{"ChannelId":"c1","UserId":"u1","JoinTime":100,"LeaveTime":200}"#
        );
    }

    /// The tag that is visible on the field is the one not to copy.
    #[test]
    fn user_email_is_not_named_after_its_column() {
        let result = ChannelMemberHistoryResult {
            user_email: "user@example.com".into(),
            ..Default::default()
        };
        let json = go_json_marshal(&result).unwrap();
        assert!(json.contains(r#""UserEmail":"user@example.com""#), "{json}");
        assert!(!json.contains(r#""Email""#), "the db tag leaked: {json}");
    }

    /// Three states, not two — which is why `leave_time` is an `Option` and not a bare `i64`.
    #[test]
    fn leave_time_distinguishes_absent_from_zero() {
        let still_here = ChannelMemberHistory {
            join_time: 1,
            ..Default::default()
        };
        let left_at_epoch = ChannelMemberHistory {
            join_time: 1,
            leave_time: Some(0),
            ..Default::default()
        };

        assert_ne!(still_here, left_at_epoch);
        assert!(
            go_json_marshal(&still_here)
                .unwrap()
                .contains(r#""LeaveTime":null"#)
        );
        assert!(
            go_json_marshal(&left_at_epoch)
                .unwrap()
                .contains(r#""LeaveTime":0"#)
        );
    }

    #[test]
    fn the_zero_values_are_full_objects() {
        assert_eq!(
            go_json_marshal(&ChannelMemberHistory::default()).unwrap(),
            r#"{"ChannelId":"","UserId":"","JoinTime":0,"LeaveTime":null}"#
        );
        assert_eq!(
            go_json_marshal(&ChannelMemberHistoryResult::default()).unwrap(),
            r#"{"ChannelId":"","UserId":"","JoinTime":0,"LeaveTime":null,"UserEmail":"","Username":"","IsBot":false,"UserDeleteAt":0}"#
        );
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: ChannelMemberHistory = serde_json::from_str(r#"{"JoinTime":5}"#).unwrap();
        assert_eq!(got.join_time, 5);
        assert!(got.leave_time.is_none() && got.channel_id.is_empty());
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/channel_member_history.json");
        let decoded: ChannelMemberHistory = serde_json::from_str(raw).unwrap();
        assert!(!decoded.channel_id.is_empty() && decoded.leave_time.is_some());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/channel_member_history_result.json");
        let decoded: ChannelMemberHistoryResult = serde_json::from_str(raw).unwrap();
        assert!(!decoded.user_email.is_empty() && decoded.leave_time.is_some());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_member_history.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_member_history.json"
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

    /// The key lists, read off the Go struct tags by the oracle. Order matters as well as
    /// membership: it is the emission order.
    ///
    /// This is the test the module exists for — with no `json:` tags there is nothing else to get
    /// wrong, and a field renamed or added upstream fails here rather than silently changing the
    /// wire format.
    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();

        assert_eq!(
            keys(&oracle, "history_keys"),
            ["ChannelId", "UserId", "JoinTime", "LeaveTime"]
        );
        assert_eq!(
            keys(&oracle, "result_keys"),
            [
                "ChannelId",
                "UserId",
                "JoinTime",
                "LeaveTime",
                // Not `Email` — the `db` tag is invisible to encoding/json.
                "UserEmail",
                "Username",
                "IsBot",
                "UserDeleteAt",
            ]
        );

        // And the same lists as our types actually emit, so the assertions above are not merely
        // a restatement of the fixture.
        let emitted = |json: &str| -> Vec<String> {
            serde_json::from_str::<serde_json::Map<String, Value>>(json)
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        // `serde_json::Map` sorts, so compare as sets against a sorted copy of Go's list.
        let mut want = keys(&oracle, "history_keys");
        want.sort();
        assert_eq!(
            emitted(&go_json_marshal(&ChannelMemberHistory::default()).unwrap()),
            want
        );

        let mut want = keys(&oracle, "result_keys");
        want.sort();
        assert_eq!(
            emitted(&go_json_marshal(&ChannelMemberHistoryResult::default()).unwrap()),
            want
        );
    }

    #[test]
    fn the_history_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["history_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 7, "the history corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: ChannelMemberHistory =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert_eq!(
                decoded.leave_time.is_none(),
                case["leave_time_nil"].as_bool().unwrap(),
                "{name}: leave_time nil"
            );
        }
    }

    #[test]
    fn the_result_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["result_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the result corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: ChannelMemberHistoryResult =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert_eq!(
                decoded.leave_time.is_none(),
                case["leave_time_nil"].as_bool().unwrap(),
                "{name}: leave_time nil"
            );
        }
    }

    /// The only pointer in either struct. `null` is accepted by both sides here — unlike every
    /// other [D-057] case, because the Rust field is an `Option` and `null` is its `None`.
    #[test]
    fn the_leave_time_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["leave_time_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the leave-time corpus changed size");

        let mut null_seen = false;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<ChannelMemberHistory>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");

            if !go_ok {
                // Go allocated the pointer and then failed to fill it, so the receiver holds a
                // non-nil pointer to zero *and* an error. Not comparable from Rust, where a
                // decode is all or nothing; pinned because a Go handler ignoring the error would
                // read it as "left at the epoch" rather than "still present".
                assert!(!case["leave_time_nil"].as_bool().unwrap(), "{name}");
                continue;
            }

            let got = got.unwrap();
            assert_eq!(
                got.leave_time.is_none(),
                case["leave_time_nil"].as_bool().unwrap(),
                "{name}: nil-ness"
            );
            assert_eq!(got.leave_time, case["leave_time"].as_i64(), "{name}: value");
            assert_eq!(
                got.join_time,
                case["join_time_after"].as_i64().unwrap(),
                "{name}"
            );

            if name == "null" {
                null_seen = true;
                // The one place `null` into a scalar is *not* [D-057]: the field is nillable in
                // Go too, so `Option<i64>` reproduces it exactly.
                assert!(got.leave_time.is_none());
            }
        }
        assert!(null_seen);
    }

    /// [D-040] at its widest: with no `json:` tags, Go's case-insensitive fallback means every
    /// key of this type has three spellings we reject and it accepts.
    #[test]
    fn only_the_declared_key_casing_decodes_here() {
        let oracle = oracle();
        let cases = oracle["key_casing_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the casing corpus changed size");

        let (mut divergent, mut agreed) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let decoded: ChannelMemberHistory =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            let ours = !decoded.channel_id.is_empty();
            let theirs = case["populated"].as_bool().unwrap();

            if name == "declared" {
                assert!(
                    ours && theirs,
                    "the declared spelling must work on both sides"
                );
                agreed += 1;
            } else if theirs {
                // Go's case-insensitive fallback.
                assert!(
                    !ours,
                    "{name}: we accepted a spelling [D-040] says we reject"
                );
                divergent += 1;
            } else {
                // Go rejects it too: the fallback folds case, not punctuation.
                assert!(!ours, "{name}");
                agreed += 1;
            }
        }

        assert_eq!(divergent, 3, "the [D-040] spellings changed count");
        assert_eq!(agreed, 3, "declared, plus the two Go also rejects");
    }
}
