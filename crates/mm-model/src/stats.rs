//! Port of `model/team_stats.go` (10 lines), `model/users_stats.go` (8) and
//! `model/cluster_stats.go` (13) — **all three whole**.
//!
//! Eight tagged fields between them, no methods, no pointers, no `omitempty`. They are one module
//! because there is one question across all three and it is the same question three times.
//!
//! # Go's `int` is platform-width, and `ClusterStats` uses it where the others use `int64`
//!
//! ```go
//! TeamStats.TotalMemberCount             int64
//! UsersStats.TotalUsersCount             int64
//! ClusterStats.TotalWebsocketConnections int    // <- not int64
//! ```
//!
//! The crate has mapped every Go integer to `i64`, and for `int` that is right **only if the wire
//! accepts the full 64-bit range** — a property of the host the server was built for, not of the
//! type declaration. So it was measured rather than assumed: the oracle records
//! `strconv.IntSize` (64 on the generating host) and drives eleven numeric bounds through an
//! `int` field and an `int64` field side by side. They agree on all eleven, including both
//! `int64` extremes and the two values just past them, which is what justifies `i64` here.
//!
//! On a 32-bit build they would part company at 2^31 and Go would reject a value we accept. That
//! is [D-074] — logged rather than handled, because Mattermost ships no 32-bit server and
//! handling it would mean a platform-conditional wire type.
//!
//! # Otherwise these are the plainest types in the crate
//!
//! Nothing validates, nothing is nillable, nothing is optional. Each zero value is a full object
//! rather than `{}`, and the only divergences are the two standing crate-wide ones.

use serde::{Deserialize, Serialize};

/// Port of `model.TeamStats` (team_stats.go:6).
///
/// Nothing enforces `active <= total`; both are whatever the store counted.
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamStats {
    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "total_member_count")]
    pub total_member_count: i64,

    #[serde(rename = "active_member_count")]
    pub active_member_count: i64,
}

/// Port of `model.UsersStats` (users_stats.go:6).
///
/// One field. It exists as a type rather than a bare number because it is a response body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsersStats {
    #[serde(rename = "total_users_count")]
    pub total_users_count: i64,
}

/// Port of `model.ClusterStats` (cluster_stats.go:6).
///
/// **The three counts are Go `int`, not `int64`.** `i64` is correct on a 64-bit host and is
/// measured as such; see the module docs and [D-074].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterStats {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "total_websocket_connections")]
    pub total_websocket_connections: i64,

    #[serde(rename = "total_read_db_connections")]
    pub total_read_db_connections: i64,

    #[serde(rename = "total_master_db_connections")]
    pub total_master_db_connections: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    #[test]
    fn the_zero_values_are_full_objects() {
        assert_eq!(
            go_json_marshal(&TeamStats::default()).unwrap(),
            r#"{"team_id":"","total_member_count":0,"active_member_count":0}"#
        );
        assert_eq!(
            go_json_marshal(&UsersStats::default()).unwrap(),
            r#"{"total_users_count":0}"#
        );
        assert_eq!(
            go_json_marshal(&ClusterStats::default()).unwrap(),
            r#"{"id":"","total_websocket_connections":0,"total_read_db_connections":0,"total_master_db_connections":0}"#
        );
    }

    /// Nothing validates, so an active count above the total round-trips unchanged.
    #[test]
    fn nothing_is_validated() {
        let stats = TeamStats {
            team_id: "t".into(),
            total_member_count: 1,
            active_member_count: 99,
        };
        assert_eq!(
            go_json_marshal(&stats).unwrap(),
            r#"{"team_id":"t","total_member_count":1,"active_member_count":99}"#
        );
    }

    /// The `int` fields carry the full `i64` range, which is what the oracle measured.
    #[test]
    fn the_cluster_counts_hold_the_full_int64_range() {
        let stats = ClusterStats {
            id: "n".into(),
            total_websocket_connections: i64::MAX,
            total_read_db_connections: i64::MIN,
            total_master_db_connections: 0,
        };
        let json = go_json_marshal(&stats).unwrap();
        assert!(json.contains("9223372036854775807"), "{json}");
        assert!(json.contains("-9223372036854775808"), "{json}");
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: TeamStats = serde_json::from_str(r#"{"team_id":"t1"}"#).unwrap();
        assert_eq!(got.team_id, "t1");
        assert_eq!(got.total_member_count, 0);

        let got: ClusterStats = serde_json::from_str("{}").unwrap();
        assert_eq!(got, ClusterStats::default());
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/team_stats.json");
        let decoded: TeamStats = serde_json::from_str(raw).unwrap();
        assert!(!decoded.team_id.is_empty() && decoded.total_member_count != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/users_stats.json");
        let decoded: UsersStats = serde_json::from_str(raw).unwrap();
        assert!(decoded.total_users_count != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/cluster_stats.json");
        let decoded: ClusterStats = serde_json::from_str(raw).unwrap();
        assert!(!decoded.id.is_empty() && decoded.total_websocket_connections != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_stats.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_stats.json")).unwrap()
    }

    /// Go accepts `null` into a scalar and leaves the zero value; we reject. See [D-057].
    const NULL_SCALAR: [&str; 2] = ["null_string", "null_int"];

    /// Go matches field names case-insensitively; we do not. See [D-040].
    const CASE_FOLDED: [&str; 2] = ["uppercase_key", "mixed_case_key"];

    /// A repeated struct field: Go takes the last value, serde's derive errors. See [D-071].
    const DUPLICATE_FIELD: &str = "duplicate_key";

    /// The question the module exists for, and the reason `i64` is a measurement rather than a
    /// habit: does Go's `int` accept everything its `int64` does?
    ///
    /// On the generating host, yes — all eleven bounds agree, including both `int64` extremes.
    /// The `int_size` assertion is what stops this test silently meaning something weaker if the
    /// fixture is ever regenerated on a 32-bit builder.
    #[test]
    fn go_int_and_go_int64_agree_on_this_host() {
        let oracle = oracle();

        assert_eq!(
            oracle["int_size"].as_u64().unwrap(),
            64,
            "the fixture was generated on a host where Go's int is not 64-bit; [D-074] applies \
             and `i64` is no longer the right mapping for ClusterStats"
        );

        let cases = oracle["int_bounds"].as_array().unwrap();
        assert_eq!(cases.len(), 11, "the bounds corpus changed size");

        let (mut accepted, mut rejected) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            assert!(
                case["agree"].as_bool().unwrap(),
                "{name}: Go's int and int64 disagree — see [D-074]"
            );

            let raw = case["raw"].as_str().unwrap();
            let go_ok = case["int_ok"].as_bool().unwrap();

            // The same literal through our `i64` field, which is what the agreement licenses.
            let doc = format!(r#"{{"total_websocket_connections":{raw}}}"#);
            let got = serde_json::from_str::<ClusterStats>(&doc);
            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");

            if go_ok {
                accepted += 1;
                assert_eq!(
                    got.unwrap().total_websocket_connections,
                    case["int_value"].as_i64().unwrap(),
                    "{name}"
                );
            } else {
                rejected += 1;
            }
        }

        assert_eq!(
            (accepted, rejected),
            (9, 2),
            "the accept/reject split moved"
        );
    }

    fn check_wire<T>(cases: &[Value], label: &str, expected: usize)
    where
        T: serde::de::DeserializeOwned + serde::Serialize,
    {
        assert_eq!(cases.len(), expected, "the {label} corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(
                !case["panicked"].as_bool().unwrap(),
                "{label}/{name}: Go panicked"
            );

            let want = case["json"].as_str().unwrap();
            let decoded: T =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{label}/{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{label}/{name}");
        }
    }

    #[test]
    fn the_wire_formats_match_go() {
        let oracle = oracle();
        check_wire::<TeamStats>(oracle["team_wire"].as_array().unwrap(), "team", 6);
        check_wire::<UsersStats>(oracle["users_wire"].as_array().unwrap(), "users", 5);
        check_wire::<ClusterStats>(oracle["cluster_wire"].as_array().unwrap(), "cluster", 6);
    }

    /// The ordinary decode shape, driven through `TeamStats` because it has both a string and two
    /// integers so a document can be malformed in either position.
    ///
    /// Three named groups diverge and all three are crate-wide entries, not new here.
    #[test]
    fn the_scalar_decode_matches_go() {
        let oracle = oracle();
        let cases = oracle["scalar_decode"].as_array().unwrap();
        assert_eq!(cases.len(), 15, "the decode corpus changed size");

        let (mut agreed, mut divergent) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let got = serde_json::from_str::<TeamStats>(doc);
            let go_ok = case["ok"].as_bool().unwrap();

            if NULL_SCALAR.contains(&name) || name == DUPLICATE_FIELD {
                assert!(go_ok, "{name}: Go used to accept it");
                assert!(got.is_err(), "{name}: expected the documented divergence");
                divergent += 1;
                continue;
            }

            if CASE_FOLDED.contains(&name) {
                assert!(go_ok, "{name}: Go used to accept it");
                let got = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                // Go populated a field from the folded key; we ignored it as unknown.
                assert_eq!(got, TeamStats::default(), "{name}: expected the divergence");
                assert_ne!(
                    (
                        case["team_id"].as_str().unwrap(),
                        case["total_member_count"].as_i64().unwrap()
                    ),
                    ("", 0),
                    "{name}: Go stopped folding this key"
                );
                divergent += 1;
                continue;
            }

            assert_eq!(got.is_ok(), go_ok, "{name}: {doc}");
            agreed += 1;
            if !go_ok {
                continue;
            }

            let got = got.unwrap();
            assert_eq!(got.team_id, case["team_id"].as_str().unwrap(), "{name}");
            assert_eq!(
                got.total_member_count,
                case["total_member_count"].as_i64().unwrap(),
                "{name}"
            );
            assert_eq!(
                got.active_member_count,
                case["active_member_count"].as_i64().unwrap(),
                "{name}"
            );
            assert_eq!(
                go_json_marshal(&got).unwrap(),
                case["json_after"].as_str().unwrap(),
                "{name}"
            );
        }

        assert_eq!(divergent, 5, "the crate-wide divergences changed count");
        assert_eq!(agreed, 10);
    }

    /// `mixed_case_key` is worth its own assertion, because it looks like a counterexample to the
    /// bound [D-040] carries and is not.
    ///
    /// `Total_Member_Count` **does** populate the field in Go, even though
    /// `channel_member_history`'s corpus showed `channel_id` failing to populate `ChannelId`. Both
    /// are the same rule: Go folds **case** against the effective name, and the effective name
    /// here is the `json:` tag `total_member_count`, which already contains the underscores. Where
    /// there is no tag the effective name is `ChannelId`, which no underscored spelling folds to.
    #[test]
    fn the_case_fold_is_against_the_tag_not_the_field_name() {
        let oracle = oracle();
        let case = oracle["scalar_decode"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "mixed_case_key")
            .expect("mixed_case_key is missing");

        assert_eq!(case["in"].as_str().unwrap(), r#"{"Total_Member_Count":5}"#);
        assert_eq!(
            case["total_member_count"].as_i64().unwrap(),
            5,
            "Go folded it"
        );

        let ours: TeamStats = serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
        assert_eq!(ours.total_member_count, 0, "we treat it as an unknown key");
    }
}
