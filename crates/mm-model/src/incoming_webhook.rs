//! Port of `model/incoming_webhook.go` — the stored hook and the payload posted to it.
//!
//! # The decoder retries on invalid JSON
//!
//! Incoming webhooks are written by third parties and routinely contain **raw newlines and tabs
//! inside JSON strings**, which is invalid JSON. Go decodes once, and only on failure re-runs the
//! payload through [`escape_control_chars_from_payload`] and decodes again. That fallback is not
//! a nicety — dropping it would reject payloads the Go server accepts today.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use regex::Regex;

use crate::message_attachment::{MessageAttachment, stringify_message_attachment_field_value};
use crate::post::{
    POST_PROPS_ADAPTIVE_CARDS, POST_PROPS_ATTACHMENTS, POST_PROPS_BLOCK_KIT_BLOCKS,
    POST_PROPS_MM_BLOCKS,
};
use crate::post_metadata::PostPriority;
use crate::utils::{AppError, AppResult, StringInterface, get_millis, is_valid_id, new_id};

/// Port of `model.DefaultWebhookUsername` (incoming_webhook.go:14).
pub const DEFAULT_WEBHOOK_USERNAME: &str = "webhook";

/// The inline length caps in `IsValid`, named so the branches cannot drift.
pub const INCOMING_WEBHOOK_DISPLAY_NAME_MAX_LENGTH: usize = 64;
pub const INCOMING_WEBHOOK_DESCRIPTION_MAX_LENGTH: usize = 500;
pub const INCOMING_WEBHOOK_USERNAME_MAX_LENGTH: usize = 64;
pub const INCOMING_WEBHOOK_ICON_URL_MAX_LENGTH: usize = 1024;

/// Port of `model.IncomingWebhook` (incoming_webhook.go:17).
///
/// Note `Auditable` spells one of its keys **`icon_url:`**, with a trailing colon — a Go typo
/// that reaches the audit log. Auditing is [D-028] and is not ported, so the typo has no effect
/// here; it is recorded because a future audit port would otherwise "fix" it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncomingWebhook {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    /// Overrides the post's username; `DEFAULT_WEBHOOK_USERNAME` when empty.
    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    /// When set, a payload may not redirect the post to another channel.
    #[serde(rename = "channel_locked")]
    pub channel_locked: bool,

    /// Epoch milliseconds.
    #[serde(rename = "last_used")]
    pub last_used: i64,
}

impl IncomingWebhook {
    /// Port of `(*IncomingWebhook).IsValid` (incoming_webhook.go:88).
    ///
    /// The **id branch is the only one with i18n params** (it interpolates `Id`), and only
    /// `create_at`/`update_at` carry details. All four length caps are **bytes**.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            let mut params = std::collections::HashMap::new();
            params.insert("Id".to_string(), serde_json::Value::String(self.id.clone()));
            return Err(Box::new(AppError::new(
                "IncomingWebhook.IsValid",
                "model.incoming_hook.id.app_error",
                Some(params),
                "",
                400,
            )));
        }

        if self.create_at == 0 {
            return Err(err("create_at", format!("id={}", self.id)));
        }

        if self.update_at == 0 {
            return Err(err("update_at", format!("id={}", self.id)));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", String::new()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", String::new()));
        }

        if !is_valid_id(&self.team_id) {
            return Err(err("team_id", String::new()));
        }

        if self.display_name.len() > INCOMING_WEBHOOK_DISPLAY_NAME_MAX_LENGTH {
            return Err(err("display_name", String::new()));
        }

        if self.description.len() > INCOMING_WEBHOOK_DESCRIPTION_MAX_LENGTH {
            return Err(err("description", String::new()));
        }

        if self.username.len() > INCOMING_WEBHOOK_USERNAME_MAX_LENGTH {
            return Err(err("username", String::new()));
        }

        if self.icon_url.len() > INCOMING_WEBHOOK_ICON_URL_MAX_LENGTH {
            return Err(err("icon_url", String::new()));
        }

        Ok(())
    }

    /// Port of `(*IncomingWebhook).PreSave` (incoming_webhook.go:129) — `create_at` is set
    /// unconditionally.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*IncomingWebhook).PreUpdate` (incoming_webhook.go:139).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }
}

/// The error ids are `model.incoming_hook.*` — **`incoming_hook`**, singular and without `web`.
fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "IncomingWebhook.IsValid",
        format!("model.incoming_hook.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.IncomingWebhookRequest` (incoming_webhook.go:51) — the third-party payload.
///
/// **`ChannelName` is tagged `channel`**, not `channel_name`. `Priority` is a nullable
/// `*PostPriority`, and `Silent` is persisted as the `PostPropsSilentNotification` prop rather
/// than as a column.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncomingWebhookRequest {
    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    /// Tagged `channel`.
    #[serde(rename = "channel")]
    pub channel_name: String,

    #[serde(rename = "root_id")]
    pub root_id: String,

    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<MessageAttachment>>,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "icon_emoji")]
    pub icon_emoji: String,

    #[serde(rename = "priority")]
    pub priority: Option<PostPriority>,

    /// Requests notification-suppressed delivery.
    #[serde(rename = "silent")]
    pub silent: bool,
}

impl IncomingWebhookRequest {
    /// Port of `(*IncomingWebhookRequest).HasInteractiveMessageProps`
    /// (incoming_webhook.go:71).
    ///
    /// `props.attachments` **always** counts. `mm_blocks`, `blocks` and `cards` count only when
    /// the Interactive Messages feature flag is on — so the same payload answers differently on
    /// two servers, by design.
    pub fn has_interactive_message_props(&self, mm_blocks_enabled: bool) -> bool {
        let Some(props) = &self.props else {
            return false;
        };
        if props.is_empty() {
            return false;
        }

        let non_empty = |key: &str| {
            crate::post_interactive_blocks::interactive_prop_json_array(props.get(key))
                .is_some_and(|a| !a.is_empty())
        };

        if non_empty(POST_PROPS_ATTACHMENTS) {
            return true;
        }

        if !mm_blocks_enabled {
            return false;
        }

        non_empty(POST_PROPS_MM_BLOCKS)
            || non_empty(POST_PROPS_BLOCK_KIT_BLOCKS)
            || non_empty(POST_PROPS_ADAPTIVE_CARDS)
    }

    /// Port of `model.IncomingWebhookRequestFromJSON` (incoming_webhook.go:213).
    ///
    /// Decodes, and on failure retries the escaped payload — see the module docs. The attachments
    /// are then stringified exactly as `CommandResponseFromJSON` does.
    pub fn from_json(data: &[u8]) -> AppResult<IncomingWebhookRequest> {
        let mut request: IncomingWebhookRequest = match serde_json::from_slice(data) {
            Ok(request) => request,
            Err(_) => {
                let escaped = escape_control_chars_from_payload(data);
                serde_json::from_slice(&escaped).map_err(|e| {
                    Box::new(
                        AppError::new(
                            "IncomingWebhookRequestFromJSON",
                            "model.incoming_hook.parse_data.app_error",
                            None,
                            "",
                            400,
                        )
                        .wrap(e),
                    )
                })?
            }
        };

        if let Some(attachments) = request.attachments.take() {
            request.attachments = Some(stringify_message_attachment_field_value(attachments));
        }

        Ok(request)
    }
}

/// The six keys whose values are repaired. A control character anywhere else in the payload is
/// still a parse error.
static PAYLOAD_STRING_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    // (?s) so `.` — and the negated class — may span newlines, which is the whole point.
    Regex::new(r#"(?s)"(text|fallback|pretext|author_name|title|value)"\s*:\s*"(\\"|[^"])*""#)
        .unwrap_or_else(|e| unreachable!("literal pattern: {e}"))
});

/// Port of `escapeControlCharsFromPayload` (incoming_webhook.go:169).
///
/// Finds `"<key>": "<value>"` pairs for six known keys and escapes raw `\n` and `\t` **inside the
/// match only**. Go works on bytes; this works on the UTF-8 text and falls back to returning the
/// input unchanged when it is not valid UTF-8 — which Go's regex, being byte-oriented, would have
/// processed. That difference is unreachable for a JSON payload, which must be UTF-8 to decode
/// either way.
pub fn escape_control_chars_from_payload(by: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(by) else {
        return by.to_vec();
    };

    PAYLOAD_STRING_VALUE
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let matched = caps.get(0).map_or("", |m| m.as_str());
            matched.replace('\n', "\\n").replace('\t', "\\t")
        })
        .into_owned()
        .into_bytes()
}

/// Port of `model.IncomingWebhooksWithCount` (incoming_webhook.go:84).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncomingWebhooksWithCount {
    #[serde(rename = "incoming_webhooks")]
    pub webhooks: Option<Vec<IncomingWebhook>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
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
    fn incoming_webhook_round_trips_the_fixture() {
        assert_fixture_round_trips!(IncomingWebhook, "incoming_webhook");
    }
    #[test]
    fn incoming_webhook_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(IncomingWebhookRequest, "incoming_webhook_request");
    }
    #[test]
    fn incoming_webhooks_with_count_round_trips_the_fixture() {
        assert_fixture_round_trips!(IncomingWebhooksWithCount, "incoming_webhooks_with_count");
    }
}
