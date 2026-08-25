//! Port of `model/recap.go` — an AI-generated summary of one or more channels.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_str, is_none_or_empty_vec};

pub const RECAP_STATUS_PENDING: &str = "pending";
pub const RECAP_STATUS_PROCESSING: &str = "processing";
pub const RECAP_STATUS_COMPLETED: &str = "completed";
pub const RECAP_STATUS_FAILED: &str = "failed";
/// Skipped because of a limit violation or a non-recoverable creation failure — distinct from
/// [`RECAP_STATUS_FAILED`], which means the generation itself failed.
pub const RECAP_STATUS_SKIPPED: &str = "skipped";

pub const SKIP_REASON_DAILY_LIMIT: &str = "daily_limit_reached";
pub const SKIP_REASON_COOLDOWN: &str = "cooldown_active";
/// The recap row was committed but its processing job could not be enqueued — so a recap with
/// this reason exists and will never be filled in.
pub const SKIP_REASON_JOB_CREATION_FAILED: &str = "job_creation_failed";

/// Port of `model.Recap` (recap.go:3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recap {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "title")]
    pub title: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// When the user marked it read.
    #[serde(rename = "read_at")]
    pub read_at: i64,

    /// When the user last opened it — distinct from `read_at`.
    #[serde(rename = "viewed_at")]
    pub viewed_at: i64,

    #[serde(rename = "total_message_count")]
    pub total_message_count: i64,

    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "bot_id")]
    pub bot_id: String,

    /// Set only when this recap came from a schedule.
    #[serde(rename = "scheduled_recap_id", skip_serializing_if = "is_empty_str")]
    pub scheduled_recap_id: String,

    /// One of the `SKIP_REASON_*` constants.
    #[serde(rename = "skip_reason", skip_serializing_if = "is_empty_str")]
    pub skip_reason: String,

    #[serde(rename = "channels", skip_serializing_if = "is_none_or_empty_vec")]
    pub channels: Option<Vec<RecapChannel>>,
}

/// Port of `model.RecapChannel` (recap.go:20) — one channel's section of a recap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecapChannel {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "recap_id")]
    pub recap_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Denormalised at generation time, so the recap still reads correctly after a rename.
    #[serde(rename = "channel_name")]
    pub channel_name: String,

    #[serde(rename = "highlights")]
    pub highlights: Option<Vec<String>>,

    #[serde(rename = "action_items")]
    pub action_items: Option<Vec<String>>,

    /// The posts the summary was drawn from, for attribution.
    #[serde(rename = "source_post_ids")]
    pub source_post_ids: Option<Vec<String>>,

    #[serde(rename = "create_at")]
    pub create_at: i64,
}

/// Port of `model.CreateRecapRequest` (recap.go:31).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateRecapRequest {
    #[serde(rename = "title")]
    pub title: String,

    #[serde(rename = "channel_ids")]
    pub channel_ids: Option<Vec<String>>,

    #[serde(rename = "agent_id")]
    pub agent_id: String,
}

/// Port of `model.AIRecapSummaryResponse` (recap.go:37) — what the agent returns per channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AIRecapSummaryResponse {
    #[serde(rename = "highlights")]
    pub highlights: Option<Vec<String>>,

    #[serde(rename = "action_items")]
    pub action_items: Option<Vec<String>>,
}

/// Port of `model.RecapProcessingOptions` (recap.go:42). No tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecapProcessingOptions {
    /// One of the `TIME_PERIOD_*` constants in `scheduled_recap.rs`.
    pub time_period: String,
    pub custom_instructions: String,
}

/// Port of `model.RecapChannelResult` (recap.go:48). No tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecapChannelResult {
    pub channel_id: String,
    pub message_count: i64,
    pub success: bool,
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
    fn recap_round_trips_the_fixture() {
        assert_fixture_round_trips!(Recap, "recap");
    }
    #[test]
    fn recap_channel_round_trips_the_fixture() {
        assert_fixture_round_trips!(RecapChannel, "recap_channel");
    }
    #[test]
    fn create_recap_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(CreateRecapRequest, "create_recap_request");
    }
    #[test]
    fn ai_recap_summary_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(AIRecapSummaryResponse, "ai_recap_summary_response");
    }
}
