//! Port of `model/command_response.go` — what a slash command's endpoint sends back.

use serde::{Deserialize, Serialize};

use crate::go_url::parse_request_uri;
use crate::message_attachment::{MessageAttachment, stringify_message_attachment_field_value};
use crate::utils::{AppError, AppResult, StringInterface};

/// Port of `model.CommandResponseTypeInChannel` (command_response.go:15).
pub const COMMAND_RESPONSE_TYPE_IN_CHANNEL: &str = "in_channel";
/// Port of `model.CommandResponseTypeEphemeral` (command_response.go:16).
pub const COMMAND_RESPONSE_TYPE_EPHEMERAL: &str = "ephemeral";

/// Port of `model.CommandResponse` (command_response.go:19).
///
/// **No field carries `omitempty`**, so every key is always written — including `attachments` and
/// `extra_responses` as `null`, and `props` as `null`.
///
/// Go's `[]*MessageAttachment` and `[]*CommandResponse` can contain nil elements, and both
/// `IsValid` and `CommandResponseFromJSON` skip them explicitly. `Vec<T>` of values cannot hold a
/// nil, so those guards have no counterpart here — a JSON `null` inside the array is a decode
/// error rather than a silently skipped element. That is the only behavioural difference in this
/// file and it is the safe direction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandResponse {
    /// [`COMMAND_RESPONSE_TYPE_IN_CHANNEL`], [`COMMAND_RESPONSE_TYPE_EPHEMERAL`], or empty —
    /// which is valid and means "use the default".
    #[serde(rename = "response_type")]
    pub response_type: String,

    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    /// The **post** type to create, e.g. `custom_xyz`. Unrelated to `response_type`.
    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "goto_location")]
    pub goto_location: String,

    #[serde(rename = "trigger_id")]
    pub trigger_id: String,

    /// Set to skip the Slack-compatibility rewriting of `text`.
    #[serde(rename = "skip_slack_parsing")]
    pub skip_slack_parsing: bool,

    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<MessageAttachment>>,

    #[serde(rename = "extra_responses")]
    pub extra_responses: Option<Vec<CommandResponse>>,
}

impl CommandResponse {
    /// Port of `model.CommandResponseFromPlainText` (command_response.go:47) — a bare body with
    /// everything else zero.
    pub fn from_plain_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    /// Port of `model.CommandResponseFromJSON` (command_response.go:53).
    ///
    /// The decode is followed by [`stringify_message_attachment_field_value`] over the response's
    /// own attachments **and** over each extra response's — one level deep only, which is what Go
    /// does. A third level is left alone.
    ///
    /// Go wraps the decode error with `utils.HumanizeJSONError`, which annotates it with the
    /// offending line and column; `serde_json::Error` already carries both.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        let mut o: CommandResponse = serde_json::from_slice(data)?;

        if let Some(attachments) = o.attachments.take() {
            o.attachments = Some(stringify_message_attachment_field_value(attachments));
        }

        if let Some(extra) = o.extra_responses.as_mut() {
            for resp in extra.iter_mut() {
                if let Some(attachments) = resp.attachments.take() {
                    resp.attachments = Some(stringify_message_attachment_field_value(attachments));
                }
            }
        }

        Ok(o)
    }

    /// Port of `model.CommandResponseFromHTTPBody` (command_response.go:37).
    ///
    /// The content-type test is `strings.Split(contentType, ";")[0]` trimmed — so
    /// `application/json; charset=utf-8` is JSON and ` APPLICATION/JSON ` is **not**: the compare
    /// is case-sensitive.
    ///
    /// Go's version reads from an `io.Reader` and returns `(nil, nil)` when the read fails —
    /// a nil response *and* a nil error, which the caller must not dereference. Taking a `&[u8]`
    /// removes that state: the read has already happened.
    pub fn from_http_body(content_type: &str, body: &[u8]) -> Result<Self, serde_json::Error> {
        let first = content_type.split(';').next().unwrap_or("").trim();
        if first == "application/json" {
            return Self::from_json(body);
        }
        Ok(Self::from_plain_text(String::from_utf8_lossy(body)))
    }

    /// Port of `(*CommandResponse).IsValid` (command_response.go:76).
    ///
    /// Three URL rules that are **not** the same rule:
    ///
    /// - `icon_url` must parse *and* be `http`/`https`;
    /// - `goto_location` must only parse — any scheme is accepted, including `javascript:`;
    /// - neither is checked when empty.
    ///
    /// Nested responses are validated recursively and their error is returned **unwrapped**, so a
    /// failure three levels down reports the inner `Where`, not this one's.
    pub fn is_valid(&self) -> AppResult {
        if self.response_type != COMMAND_RESPONSE_TYPE_IN_CHANNEL
            && self.response_type != COMMAND_RESPONSE_TYPE_EPHEMERAL
            && !self.response_type.is_empty()
        {
            return Err(err("response_type", "invalid response type"));
        }

        if !self.icon_url.is_empty() {
            match parse_request_uri(&self.icon_url) {
                Ok(u) if u.scheme == "http" || u.scheme == "https" => {}
                _ => return Err(err("icon_url", "invalid icon url")),
            }
        }

        if !self.goto_location.is_empty() && parse_request_uri(&self.goto_location).is_err() {
            return Err(err("goto_location", "invalid goto location"));
        }

        for attachment in self.attachments.iter().flatten() {
            if attachment.is_valid().is_err() {
                return Err(err("attachment", "invalid attachment"));
            }
        }

        for resp in self.extra_responses.iter().flatten() {
            resp.is_valid()?;
        }

        Ok(())
    }
}

fn err(field: &str, details: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "CommandResponse.IsValid",
        format!("model.command_response.is_valid.{field}.app_error"),
        None,
        details,
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
    fn command_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(CommandResponse, "command_response");
    }
}
