//! Port of `model/channel_join_request.go` — a request to join a discoverable private channel.
//!
//! Rows are **append-only / status-mutating**: `pending → approved | denied | withdrawn`, and
//! nothing is ever deleted, so the audit history survives. A partial unique index in Postgres
//! enforces at most one *pending* row per `(channel_id, user_id)` — which is why this type has no
//! uniqueness check of its own.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_none;
use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id, sanitize_unicode};

pub const CHANNEL_JOIN_REQUEST_STATUS_PENDING: &str = "pending";
pub const CHANNEL_JOIN_REQUEST_STATUS_APPROVED: &str = "approved";
pub const CHANNEL_JOIN_REQUEST_STATUS_DENIED: &str = "denied";
pub const CHANNEL_JOIN_REQUEST_STATUS_WITHDRAWN: &str = "withdrawn";

pub const CHANNEL_JOIN_REQUEST_MESSAGE_MAX_RUNES: usize = 500;
pub const CHANNEL_JOIN_REQUEST_DENIAL_REASON_MAX_RUNES: usize = 500;

/// Port of `model.IsValidChannelJoinRequestStatus` (channel_join_request.go:62).
pub fn is_valid_channel_join_request_status(s: &str) -> bool {
    matches!(
        s,
        CHANNEL_JOIN_REQUEST_STATUS_PENDING
            | CHANNEL_JOIN_REQUEST_STATUS_APPROVED
            | CHANNEL_JOIN_REQUEST_STATUS_DENIED
            | CHANNEL_JOIN_REQUEST_STATUS_WITHDRAWN
    )
}

/// Port of `model.ChannelJoinRequest` (channel_join_request.go:26).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelJoinRequest {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// The requester's note. Sanitised on save; capped in **runes**.
    #[serde(rename = "message")]
    pub message: String,

    #[serde(rename = "status")]
    pub status: String,

    /// Surfaced to the requester. Only meaningful on a `denied` request — `IsValid` rejects it
    /// on any other status.
    #[serde(rename = "denial_reason")]
    pub denial_reason: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "reviewed_by")]
    pub reviewed_by: String,

    #[serde(rename = "reviewed_at")]
    pub reviewed_at: i64,
}

impl ChannelJoinRequest {
    /// Port of `(*ChannelJoinRequest).IsValid` (channel_join_request.go:88).
    ///
    /// Two cross-field rules beyond the per-field ones:
    ///
    /// - a `denial_reason` requires status `denied`;
    /// - `approved` and `denied` both require **both** `reviewed_by` and `reviewed_at`.
    ///
    /// `withdrawn` requires neither, because the requester withdraws their own request.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", details()));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", details()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", details()));
        }

        if self.update_at == 0 {
            return Err(err("update_at", details()));
        }

        if !is_valid_channel_join_request_status(&self.status) {
            return Err(err("status", details()));
        }

        if self.message.chars().count() > CHANNEL_JOIN_REQUEST_MESSAGE_MAX_RUNES {
            return Err(err("message", details()));
        }

        if self.denial_reason.chars().count() > CHANNEL_JOIN_REQUEST_DENIAL_REASON_MAX_RUNES {
            return Err(err("denial_reason", details()));
        }

        if !self.denial_reason.is_empty() && self.status != CHANNEL_JOIN_REQUEST_STATUS_DENIED {
            return Err(err("denial_reason_status", details()));
        }

        if !self.reviewed_by.is_empty() && !is_valid_id(&self.reviewed_by) {
            return Err(err("reviewed_by", details()));
        }

        if matches!(
            self.status.as_str(),
            CHANNEL_JOIN_REQUEST_STATUS_APPROVED | CHANNEL_JOIN_REQUEST_STATUS_DENIED
        ) && (self.reviewed_by.is_empty() || self.reviewed_at == 0)
        {
            return Err(err("reviewer", details()));
        }

        Ok(())
    }

    /// Port of `(*ChannelJoinRequest).PreSave` (channel_join_request.go:142).
    ///
    /// `update_at` is set to `create_at` **unconditionally**, so re-saving an existing row rewinds
    /// it. Both free-text fields go through [`sanitize_unicode`].
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        if self.status.is_empty() {
            self.status = CHANNEL_JOIN_REQUEST_STATUS_PENDING.to_string();
        }
        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
        self.message = sanitize_unicode(&self.message);
        self.denial_reason = sanitize_unicode(&self.denial_reason);
    }

    /// Port of `(*ChannelJoinRequest).PreUpdate` (channel_join_request.go:157).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
        self.message = sanitize_unicode(&self.message);
        self.denial_reason = sanitize_unicode(&self.denial_reason);
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "ChannelJoinRequest.IsValid",
        format!("model.channel_join_request.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.ChannelJoinRequestList` (channel_join_request.go:40).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelJoinRequestList {
    #[serde(rename = "requests")]
    pub requests: Option<Vec<ChannelJoinRequest>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.ChannelJoinRequestPatch` (channel_join_request.go:47) — the review action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelJoinRequestPatch {
    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "denial_reason", skip_serializing_if = "is_none")]
    pub denial_reason: Option<String>,
}

/// Port of `model.GetChannelJoinRequestsOpts` (channel_join_request.go:54). No tags.
///
/// **An empty `status` means `pending`** — not "any status".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetChannelJoinRequestsOpts {
    pub status: String,
    pub page: i64,
    pub per_page: i64,
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
    fn channel_join_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelJoinRequest, "channel_join_request");
    }
    #[test]
    fn channel_join_request_list_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelJoinRequestList, "channel_join_request_list");
    }
    #[test]
    fn channel_join_request_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelJoinRequestPatch, "channel_join_request_patch");
    }
}
