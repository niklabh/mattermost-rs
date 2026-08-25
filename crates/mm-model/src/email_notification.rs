//! Port of `model/email_notification.go` — the payload a notification-email template renders from.
//!
//! # `EmailNotificationContent` is embedded, so its eight keys are **inlined**
//!
//! `EmailNotification` ends with a bare `EmailNotificationContent`, which `encoding/json` flattens
//! into the parent object. `#[serde(flatten)]` is the equivalent; a nested key would be a wire
//! break. All eight inlined keys carry `omitempty`, so a bare notification adds nothing.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;

/// Port of `model.EmailNotificationContent` (email_notification.go:3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailNotificationContent {
    #[serde(rename = "subject", skip_serializing_if = "is_empty_str")]
    pub subject: String,

    #[serde(rename = "title", skip_serializing_if = "is_empty_str")]
    pub title: String,

    /// Tagged **`subtitle`**, one word, while the field is `SubTitle`.
    #[serde(rename = "subtitle", skip_serializing_if = "is_empty_str")]
    pub sub_title: String,

    #[serde(rename = "message_html", skip_serializing_if = "is_empty_str")]
    pub message_html: String,

    #[serde(rename = "message_text", skip_serializing_if = "is_empty_str")]
    pub message_text: String,

    #[serde(rename = "button_text", skip_serializing_if = "is_empty_str")]
    pub button_text: String,

    #[serde(rename = "button_url", skip_serializing_if = "is_empty_str")]
    pub button_url: String,

    #[serde(rename = "footer_text", skip_serializing_if = "is_empty_str")]
    pub footer_text: String,
}

/// Port of `model.EmailNotification` (email_notification.go:14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailNotification {
    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "sender_id")]
    pub sender_id: String,

    #[serde(rename = "sender_display_name", skip_serializing_if = "is_empty_str")]
    pub sender_display_name: String,

    #[serde(rename = "recipient_id")]
    pub recipient_id: String,

    #[serde(rename = "root_id", skip_serializing_if = "is_empty_str")]
    pub root_id: String,

    #[serde(rename = "channel_type")]
    pub channel_type: String,

    #[serde(rename = "channel_name")]
    pub channel_name: String,

    #[serde(rename = "team_name")]
    pub team_name: String,

    #[serde(rename = "sender_username")]
    pub sender_username: String,

    #[serde(rename = "is_direct_message")]
    pub is_direct_message: bool,

    #[serde(rename = "is_group_message")]
    pub is_group_message: bool,

    #[serde(rename = "is_thread_reply")]
    pub is_thread_reply: bool,

    /// Collapsed Reply Threads.
    #[serde(rename = "is_crt_enabled")]
    pub is_crt_enabled: bool,

    #[serde(rename = "use_military_time")]
    pub use_military_time: bool,

    /// Embedded anonymously in Go — its keys are inlined here, not nested.
    #[serde(flatten)]
    pub content: EmailNotificationContent,
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
    fn email_notification_content_round_trips_the_fixture() {
        assert_fixture_round_trips!(EmailNotificationContent, "email_notification_content");
    }
    #[test]
    fn email_notification_round_trips_the_fixture() {
        assert_fixture_round_trips!(EmailNotification, "email_notification");
    }
}
