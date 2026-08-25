//! Port of `model/data_retention_policy.go` — the global and granular retention policies.
//!
//! # `post_duration` is the same key on three different types, and only one of them is nullable
//!
//! `RetentionPolicy.PostDurationDays` is a `*int64` tagged `post_duration`, where `null` means
//! "no post-duration limit for this policy". `RetentionPolicyForTeam` and
//! `RetentionPolicyForChannel` use a plain `int64` for the same key. A client that treats all
//! three alike will read `0` where the server meant "unset".
//!
//! The `db:` tags matter too: `ID` is column **`Id`** and `PostDurationDays` is column
//! **`PostDuration`** on all three.

use serde::{Deserialize, Serialize};

use crate::utils::new_id;

/// Port of `model.GlobalRetentionPolicy` (data_retention_policy.go:3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalRetentionPolicy {
    #[serde(rename = "message_deletion_enabled")]
    pub message_deletion_enabled: bool,

    #[serde(rename = "file_deletion_enabled")]
    pub file_deletion_enabled: bool,

    /// Epoch milliseconds — anything older is deleted.
    #[serde(rename = "message_retention_cutoff")]
    pub message_retention_cutoff: i64,

    #[serde(rename = "file_retention_cutoff")]
    pub file_retention_cutoff: i64,
}

/// Port of `model.RetentionPolicy` (data_retention_policy.go:10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicy {
    /// Column `Id`.
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    /// Column `PostDuration`. `null` means unlimited — see the module docs.
    #[serde(rename = "post_duration")]
    pub post_duration_days: Option<i64>,
}

/// Port of `model.RetentionPolicyWithTeamAndChannelIDs` (data_retention_policy.go:16).
///
/// The embedded `RetentionPolicy` is **inlined** by `encoding/json`, so `id`, `display_name` and
/// `post_duration` sit beside `team_ids` at the top level.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyWithTeamAndChannelIDs {
    #[serde(flatten)]
    pub policy: RetentionPolicy,

    #[serde(rename = "team_ids")]
    pub team_ids: Option<Vec<String>>,

    #[serde(rename = "channel_ids")]
    pub channel_ids: Option<Vec<String>>,
}

/// Port of `model.RetentionPolicyWithTeamAndChannelCounts` (data_retention_policy.go:30).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyWithTeamAndChannelCounts {
    #[serde(flatten)]
    pub policy: RetentionPolicy,

    #[serde(rename = "channel_count")]
    pub channel_count: i64,

    #[serde(rename = "team_count")]
    pub team_count: i64,
}

/// Port of `model.RetentionPolicyChannel` (data_retention_policy.go:44). Join row; `db:` tags
/// only, no JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicyChannel {
    /// Column `PolicyId`.
    pub policy_id: String,
    /// Column `ChannelId`.
    pub channel_id: String,
}

/// Port of `model.RetentionPolicyTeam` (data_retention_policy.go:49).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicyTeam {
    /// Column `PolicyId`.
    pub policy_id: String,
    /// Column `TeamId`.
    pub team_id: String,
}

/// Port of `model.RetentionPolicyWithTeamAndChannelCountsList` (data_retention_policy.go:54).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyWithTeamAndChannelCountsList {
    #[serde(rename = "policies")]
    pub policies: Option<Vec<RetentionPolicyWithTeamAndChannelCounts>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.RetentionPolicyForTeam` (data_retention_policy.go:59).
///
/// The team id is selected as column **`Id`**, not `TeamId` — the row is the policy's, joined.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyForTeam {
    /// Column `Id`.
    #[serde(rename = "team_id")]
    pub team_id: String,

    /// Column `PostDuration`. **Not** nullable here, unlike [`RetentionPolicy`].
    #[serde(rename = "post_duration")]
    pub post_duration_days: i64,
}

/// Port of `model.RetentionPolicyForTeamList` (data_retention_policy.go:64).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyForTeamList {
    #[serde(rename = "policies")]
    pub policies: Option<Vec<RetentionPolicyForTeam>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.RetentionPolicyForChannel` (data_retention_policy.go:69).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyForChannel {
    /// Column `Id`.
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Column `PostDuration`.
    #[serde(rename = "post_duration")]
    pub post_duration_days: i64,
}

/// Port of `model.RetentionPolicyForChannelList` (data_retention_policy.go:74).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicyForChannelList {
    #[serde(rename = "policies")]
    pub policies: Option<Vec<RetentionPolicyForChannel>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.RetentionPolicyCursor` (data_retention_policy.go:79) — which of the three
/// policy passes the deletion job has finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionPolicyCursor {
    pub channel_policies_done: bool,
    pub team_policies_done: bool,
    pub global_policies_done: bool,
}

/// Port of `model.RetentionIdsForDeletion` (data_retention_policy.go:85).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionIdsForDeletion {
    pub id: String,
    pub table_name: String,
    pub ids: Vec<String>,
}

impl RetentionIdsForDeletion {
    /// Port of `(*RetentionIdsForDeletion).PreSave` (data_retention_policy.go:98). Sets the id
    /// only — no timestamps on this type.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
    }
}

/// Port of `model.RetentionPolicyBatchConfigs` (data_retention_policy.go:91).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionPolicyBatchConfigs {
    /// Epoch milliseconds; passed in rather than read, so a batch is reproducible.
    pub now: i64,
    pub global_policy_end_time: i64,
    pub limit: i64,
    pub preserve_pinned_posts: bool,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn global_retention_policy_round_trips_the_fixture() {
        assert_fixture_round_trips!(GlobalRetentionPolicy, "global_retention_policy");
    }
    #[test]
    fn retention_policy_round_trips_the_fixture() {
        assert_fixture_round_trips!(RetentionPolicy, "retention_policy");
    }
    #[test]
    fn retention_policy_with_team_and_channel_i_ds_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            RetentionPolicyWithTeamAndChannelIDs,
            "retention_policy_with_team_and_channel_i_ds"
        );
    }
    #[test]
    fn retention_policy_with_team_and_channel_counts_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            RetentionPolicyWithTeamAndChannelCounts,
            "retention_policy_with_team_and_channel_counts"
        );
    }
    #[test]
    fn retention_policy_with_team_and_channel_counts_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            RetentionPolicyWithTeamAndChannelCountsList,
            "retention_policy_with_team_and_channel_counts_list"
        );
    }
    #[test]
    fn retention_policy_for_team_round_trips_the_fixture() {
        assert_fixture_round_trips!(RetentionPolicyForTeam, "retention_policy_for_team");
    }
    #[test]
    fn retention_policy_for_team_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(RetentionPolicyForTeamList, "retention_policy_for_team_list");
    }
    #[test]
    fn retention_policy_for_channel_round_trips_the_fixture() {
        assert_fixture_round_trips!(RetentionPolicyForChannel, "retention_policy_for_channel");
    }
    #[test]
    fn retention_policy_for_channel_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            RetentionPolicyForChannelList,
            "retention_policy_for_channel_list"
        );
    }
}
