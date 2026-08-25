//! Port of `model/content_flagging.go` — the flag-content request bodies and their vocabulary.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;
use crate::utils::{AppError, AppResult};

/// Port of `model.ContentFlaggingGroupName` (content_flagging.go:12) — the property-group the
/// flag's fields live in.
pub const CONTENT_FLAGGING_GROUP_NAME: &str = "content_flagging";
/// Port of `model.ContentFlaggingPostType` (content_flagging.go:13) — `custom_spillage_report`,
/// built from `POST_CUSTOM_TYPE_PREFIX`.
pub const CONTENT_FLAGGING_POST_TYPE: &str = "custom_spillage_report";
/// Port of `model.ContentFlaggingBotUsername` (content_flagging.go:14).
pub const CONTENT_FLAGGING_BOT_USERNAME: &str = "content-review";

/// Port of `commentMaxRunes` (content_flagging.go:16). Unexported in Go; `pub` here because both
/// validators below are public and a caller needs the bound to build a UI.
pub const COMMENT_MAX_RUNES: usize = 1000;

/// Port of `model.AsContentReviewerParam` (content_flagging.go:18) — a query parameter.
pub const AS_CONTENT_REVIEWER_PARAM: &str = "as_content_reviewer";

/// The four statuses. **Capitalised** — `Pending`, not `pending` — unlike almost every other
/// status vocabulary in the package.
pub const CONTENT_FLAGGING_STATUS_PENDING: &str = "Pending";
pub const CONTENT_FLAGGING_STATUS_ASSIGNED: &str = "Assigned";
pub const CONTENT_FLAGGING_STATUS_REMOVED: &str = "Removed";
pub const CONTENT_FLAGGING_STATUS_RETAINED: &str = "Retained";

/// The two reviewer actions. Lower-case, unlike the statuses.
pub const CONTENT_FLAGGING_ACTION_KEEP: &str = "keep";
pub const CONTENT_FLAGGING_ACTION_REMOVE: &str = "remove";

/// Port of `model.FlagContentRequest` (content_flagging.go:31).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlagContentRequest {
    #[serde(rename = "reason")]
    pub reason: String,

    #[serde(rename = "comment", skip_serializing_if = "is_empty_str")]
    pub comment: String,
}

impl FlagContentRequest {
    /// Port of `(*FlagContentRequest).IsValid` (content_flagging.go:36).
    ///
    /// `valid_reasons` is configuration, not a constant: the admin defines the reason list, so
    /// this validator cannot check it on its own. The error ids are `api.data_spillage.error.*` —
    /// **`api.`**, not `model.`, and with no `.app_error` suffix.
    pub fn is_valid(&self, comment_required: bool, valid_reasons: &[String]) -> AppResult {
        if self.reason.is_empty() {
            return Err(err("FlagContentRequest.IsValid", "reason_required", false));
        }

        if !valid_reasons.iter().any(|r| r == &self.reason) {
            return Err(err("FlagContentRequest.IsValid", "reason_invalid", false));
        }

        if comment_required && self.comment.is_empty() {
            return Err(err("FlagContentRequest.IsValid", "comment_required", false));
        }

        if self.comment.chars().count() > COMMENT_MAX_RUNES {
            return Err(err("FlagContentRequest.IsValid", "comment_too_long", true));
        }

        Ok(())
    }
}

/// Port of `model.FlagContentActionRequest` (content_flagging.go:56).
///
/// Both fields carry `omitempty`, so an action with neither is `{}`. Nothing here validates that
/// `action` is one of the two constants — the caller does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlagContentActionRequest {
    #[serde(rename = "comment", skip_serializing_if = "is_empty_str")]
    pub comment: String,

    #[serde(rename = "action", skip_serializing_if = "is_empty_str")]
    pub action: String,
}

impl FlagContentActionRequest {
    /// Port of `(*FlagContentActionRequest).IsValid` (content_flagging.go:61).
    pub fn is_valid(&self, comment_required: bool) -> AppResult {
        if comment_required && self.comment.is_empty() {
            return Err(err(
                "FlagContentActionRequest.IsValid",
                "comment_required",
                false,
            ));
        }

        if self.comment.chars().count() > COMMENT_MAX_RUNES {
            return Err(err(
                "FlagContentActionRequest.IsValid",
                "comment_too_long",
                true,
            ));
        }

        Ok(())
    }
}

/// Only the `comment_too_long` branches carry i18n params, and both carry `MaxLength`.
fn err(where_: &'static str, id: &str, with_max_length: bool) -> Box<AppError> {
    let params = if with_max_length {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "MaxLength".to_string(),
            serde_json::Value::from(COMMENT_MAX_RUNES as i64),
        );
        Some(params)
    } else {
        None
    };

    Box::new(AppError::new(
        where_,
        format!("api.data_spillage.error.{id}"),
        params,
        "",
        400,
    ))
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
    fn flag_content_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(FlagContentRequest, "flag_content_request");
    }
    #[test]
    fn flag_content_action_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(FlagContentActionRequest, "flag_content_action_request");
    }
}
