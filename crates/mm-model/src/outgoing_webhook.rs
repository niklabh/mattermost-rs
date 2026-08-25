//! Port of `model/outgoing_webhook.go` — the stored hook, the form it posts, and the reply it
//! accepts.
//!
//! # Two length caps measure a *formatted slice*
//!
//! `IsValid` bounds `TriggerWords` and `CallbackURLs` with `len(fmt.Sprintf("%s", slice)) > 1024`
//! — the length of Go's `[a b c]` rendering, brackets and separating spaces included, **not** the
//! sum of the strings. Two callback URLs of 500 characters each are 1003 characters by this
//! measure, not 1000. [`go_format_string_slice`] reproduces the rendering so the boundary lands
//! where Go's does.
//!
//! # One error id is used for two different fields
//!
//! The `TriggerWhen > 1` branch reports **`content_type`**, copy-pasted from the branch above it.
//! Reproduced: a client keying off the id would otherwise see a different value than the Go
//! server sends.

use serde::{Deserialize, Serialize};

use crate::go_url::Values;
use crate::message_attachment::MessageAttachment;
use crate::post_metadata::PostPriority;
use crate::utils::{
    AppError, AppResult, ID_LENGTH, StringArray, StringInterface, get_millis, is_valid_http_url,
    is_valid_id, new_id,
};

/// Port of `model.OutgoingHookResponseTypeComment` (outgoing_webhook.go:80).
pub const OUTGOING_HOOK_RESPONSE_TYPE_COMMENT: &str = "comment";

/// The inline caps in `IsValid`.
pub const OUTGOING_WEBHOOK_TRIGGER_WORDS_MAX_LENGTH: usize = 1024;
pub const OUTGOING_WEBHOOK_CALLBACK_URLS_MAX_LENGTH: usize = 1024;
pub const OUTGOING_WEBHOOK_DISPLAY_NAME_MAX_LENGTH: usize = 64;
pub const OUTGOING_WEBHOOK_DESCRIPTION_MAX_LENGTH: usize = 500;
pub const OUTGOING_WEBHOOK_CONTENT_TYPE_MAX_LENGTH: usize = 128;
pub const OUTGOING_WEBHOOK_USERNAME_MAX_LENGTH: usize = 64;
pub const OUTGOING_WEBHOOK_ICON_URL_MAX_LENGTH: usize = 1024;

/// `TriggerWhen`: fire on the first word of the message.
pub const TRIGGER_WORDS_STARTS_WITH: i64 = 0;
/// `TriggerWhen`: fire only on an exact match. `IsValid` rejects anything above this.
pub const TRIGGER_WORDS_EXACT_MATCH: i64 = 1;

/// Go's `fmt.Sprintf("%s", []string)` — `[a b c]`, and `[]` for an empty slice.
///
/// Not a Mattermost function; it exists because two of `IsValid`'s bounds are measured against
/// this exact rendering. See the module docs.
pub fn go_format_string_slice(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(item);
    }
    out.push(']');
    out
}

/// Port of `model.OutgoingWebhook` (outgoing_webhook.go:15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutgoingWebhook {
    #[serde(rename = "id")]
    pub id: String,

    /// The shared secret sent in the payload so the receiver can authenticate it.
    #[serde(rename = "token")]
    pub token: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    /// **May be empty**, unlike `team_id`: an empty channel means the hook listens team-wide.
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "trigger_words")]
    pub trigger_words: Option<StringArray>,

    /// [`TRIGGER_WORDS_STARTS_WITH`] or [`TRIGGER_WORDS_EXACT_MATCH`].
    #[serde(rename = "trigger_when")]
    pub trigger_when: i64,

    #[serde(rename = "callback_urls")]
    pub callback_urls: Option<StringArray>,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "content_type")]
    pub content_type: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,
}

impl OutgoingWebhook {
    fn trigger_words_slice(&self) -> &[String] {
        self.trigger_words.as_deref().unwrap_or(&[])
    }

    fn callback_urls_slice(&self) -> &[String] {
        self.callback_urls.as_deref().unwrap_or(&[])
    }

    /// Port of `(*OutgoingWebhook).IsValid` (outgoing_webhook.go:99).
    ///
    /// Note the last two branches use ids **without** the `is_valid` segment —
    /// `model.outgoing_hook.username.app_error` and `…icon_url.app_error` — while every other
    /// branch has it. And see the module docs for the duplicated `content_type` id.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("is_valid.id", String::new()));
        }

        if self.token.len() != ID_LENGTH {
            return Err(err("is_valid.token", String::new()));
        }

        if self.create_at == 0 {
            return Err(err("is_valid.create_at", format!("id={}", self.id)));
        }

        if self.update_at == 0 {
            return Err(err("is_valid.update_at", format!("id={}", self.id)));
        }

        // The field is `CreatorId` but the error says `user_id`.
        if !is_valid_id(&self.creator_id) {
            return Err(err("is_valid.user_id", String::new()));
        }

        if !self.channel_id.is_empty() && !is_valid_id(&self.channel_id) {
            return Err(err("is_valid.channel_id", String::new()));
        }

        if !is_valid_id(&self.team_id) {
            return Err(err("is_valid.team_id", String::new()));
        }

        if go_format_string_slice(self.trigger_words_slice()).len()
            > OUTGOING_WEBHOOK_TRIGGER_WORDS_MAX_LENGTH
        {
            return Err(err("is_valid.words", String::new()));
        }

        if !self.trigger_words_slice().is_empty()
            && self.trigger_words_slice().iter().any(|w| w.is_empty())
        {
            return Err(err("is_valid.trigger_words", String::new()));
        }

        // At least one callback is required, and the rendered list is capped.
        if self.callback_urls_slice().is_empty()
            || go_format_string_slice(self.callback_urls_slice()).len()
                > OUTGOING_WEBHOOK_CALLBACK_URLS_MAX_LENGTH
        {
            return Err(err("is_valid.callback", String::new()));
        }

        for callback in self.callback_urls_slice() {
            if !is_valid_http_url(callback) {
                return Err(err("is_valid.url", String::new()));
            }
        }

        if self.display_name.len() > OUTGOING_WEBHOOK_DISPLAY_NAME_MAX_LENGTH {
            return Err(err("is_valid.display_name", String::new()));
        }

        if self.description.len() > OUTGOING_WEBHOOK_DESCRIPTION_MAX_LENGTH {
            return Err(err("is_valid.description", String::new()));
        }

        if self.content_type.len() > OUTGOING_WEBHOOK_CONTENT_TYPE_MAX_LENGTH {
            return Err(err("is_valid.content_type", String::new()));
        }

        // Go reports `content_type` here too — see the module docs.
        if self.trigger_when > TRIGGER_WORDS_EXACT_MATCH {
            return Err(err("is_valid.content_type", String::new()));
        }

        if self.username.len() > OUTGOING_WEBHOOK_USERNAME_MAX_LENGTH {
            return Err(err("username", String::new()));
        }

        if self.icon_url.len() > OUTGOING_WEBHOOK_ICON_URL_MAX_LENGTH {
            return Err(err("icon_url", String::new()));
        }

        Ok(())
    }

    /// Port of `(*OutgoingWebhook).PreSave` (outgoing_webhook.go:175).
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.token.is_empty() {
            self.token = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*OutgoingWebhook).PreUpdate` (outgoing_webhook.go:187).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*OutgoingWebhook).TriggerWordExactMatch` (outgoing_webhook.go:191).
    pub fn trigger_word_exact_match(&self, word: &str) -> bool {
        if word.is_empty() {
            return false;
        }
        self.trigger_words_slice()
            .iter()
            .any(|t| t.as_str() == word)
    }

    /// Port of `(*OutgoingWebhook).TriggerWordStartsWith` (outgoing_webhook.go:199).
    ///
    /// The test is `strings.HasPrefix(word, trigger)` — the **message word** starts with the
    /// trigger, not the other way round. An empty trigger word therefore matches everything,
    /// which is why `IsValid` rejects one.
    pub fn trigger_word_starts_with(&self, word: &str) -> bool {
        if word.is_empty() {
            return false;
        }
        self.trigger_words_slice()
            .iter()
            .any(|trigger| word.starts_with(trigger.as_str()))
    }

    /// Port of `(*OutgoingWebhook).GetTriggerWord` (outgoing_webhook.go:211) — the **first**
    /// matching trigger, or `""`.
    pub fn get_trigger_word(&self, word: &str, is_exact_match: bool) -> &str {
        if word.is_empty() {
            return "";
        }

        let found = if is_exact_match {
            self.trigger_words_slice()
                .iter()
                .find(|t| t.as_str() == word)
        } else {
            self.trigger_words_slice()
                .iter()
                .find(|trigger| word.starts_with(trigger.as_str()))
        };

        found.map_or("", String::as_str)
    }
}

fn err(suffix: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "OutgoingWebhook.IsValid",
        format!("model.outgoing_hook.{suffix}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.OutgoingWebhookPayload` (outgoing_webhook.go:65) — what the server POSTs.
///
/// **`FileIds` is a `string`, not a list**: the ids arrive space-separated in one field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutgoingWebhookPayload {
    #[serde(rename = "token")]
    pub token: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "team_domain")]
    pub team_domain: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "channel_name")]
    pub channel_name: String,

    /// Epoch **milliseconds** on the JSON wire, but seconds in the form encoding — see
    /// [`OutgoingWebhookPayload::to_form_values`].
    #[serde(rename = "timestamp")]
    pub timestamp: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "user_name")]
    pub user_name: String,

    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "trigger_word")]
    pub trigger_word: String,

    /// Space-separated ids, not a list.
    #[serde(rename = "file_ids")]
    pub file_ids: String,
}

impl OutgoingWebhookPayload {
    /// Port of `(*OutgoingWebhookPayload).ToFormValues` (outgoing_webhook.go:82).
    ///
    /// Two things to preserve: the timestamp is divided by 1000, so the form carries **seconds**
    /// where the JSON body carries milliseconds; and `url.Values.Encode` sorts by key, which
    /// `go_url::Values` reproduces — the receiving end may be verifying a signature over this
    /// exact string.
    pub fn to_form_values(&self) -> String {
        let mut v = Values::new();
        v.set("token", &self.token);
        v.set("team_id", &self.team_id);
        v.set("team_domain", &self.team_domain);
        v.set("channel_id", &self.channel_id);
        v.set("channel_name", &self.channel_name);
        v.set("timestamp", &(self.timestamp / 1000).to_string());
        v.set("user_id", &self.user_id);
        v.set("user_name", &self.user_name);
        v.set("post_id", &self.post_id);
        v.set("text", &self.text);
        v.set("trigger_word", &self.trigger_word);
        v.set("file_ids", &self.file_ids);
        v.encode()
    }
}

/// Port of `model.OutgoingWebhookResponse` (outgoing_webhook.go:80) — the reply the receiver may
/// send back to post a message.
///
/// **`Text` is a `*string`**, unlike `IncomingWebhookRequest.Text`: an absent text and an empty
/// text are different, because an absent one leaves the triggering post's text in place.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutgoingWebhookResponse {
    #[serde(rename = "text")]
    pub text: Option<String>,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<MessageAttachment>>,

    #[serde(rename = "type")]
    pub type_: String,

    /// [`OUTGOING_HOOK_RESPONSE_TYPE_COMMENT`] posts the reply as a thread reply; anything else
    /// posts it to the channel.
    #[serde(rename = "response_type")]
    pub response_type: String,

    #[serde(rename = "priority")]
    pub priority: Option<PostPriority>,
}

#[cfg(test)]
mod go_parity {
    use super::{OUTGOING_WEBHOOK_CALLBACK_URLS_MAX_LENGTH, go_format_string_slice};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// The rendering `IsValid` measures its two 1024-byte caps against. A nil slice and an empty
    /// one both render as `[]`, and every extra element costs a separating space — so the cap is
    /// not the sum of the elements' lengths.
    #[test]
    fn percent_s_over_a_string_slice_matches_go() {
        let oracle = oracle();
        let cases = oracle["slice_percent_s"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input: Vec<String> = match case["in"].as_array() {
                Some(items) => items
                    .iter()
                    .map(|i| i.as_str().unwrap().to_string())
                    .collect(),
                None => Vec::new(), // `null`, i.e. a nil slice
            };
            let rendered = go_format_string_slice(&input);
            assert_eq!(
                rendered,
                case["out"].as_str().unwrap(),
                r#"fmt.Sprintf("%s", {input:?})"#
            );
            // The number the length caps actually compare against.
            assert_eq!(rendered.len() as i64, case["len"].as_i64().unwrap());
        }
    }

    /// Two 500-character URLs are 1003 characters by Go's measure, not 1000 — the brackets and
    /// the separator count. This pins the boundary the cap sits on.
    #[test]
    fn the_callback_cap_counts_the_brackets_and_separators() {
        let five_hundred = "a".repeat(500);
        let rendered = go_format_string_slice(&[five_hundred.clone(), five_hundred]);
        assert_eq!(rendered.len(), 1003);
        assert!(rendered.len() <= OUTGOING_WEBHOOK_CALLBACK_URLS_MAX_LENGTH);
    }
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
    fn outgoing_webhook_round_trips_the_fixture() {
        assert_fixture_round_trips!(OutgoingWebhook, "outgoing_webhook");
    }
    #[test]
    fn outgoing_webhook_payload_round_trips_the_fixture() {
        assert_fixture_round_trips!(OutgoingWebhookPayload, "outgoing_webhook_payload");
    }
    #[test]
    fn outgoing_webhook_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(OutgoingWebhookResponse, "outgoing_webhook_response");
    }
}

#[cfg(test)]
mod sweep_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The four trigger-word helpers over one hook whose words **overlap** (`dep` is a prefix of
    /// `deploy`), which is what makes the exact/starts-with distinction visible: `deployment`
    /// starts with both, and Go returns the first match in declaration order, not the longest.
    #[test]
    fn trigger_word_helpers_match_go() {
        let oracle = oracle();
        let cases = oracle["outgoing_webhook_triggers"].as_array().unwrap();
        assert!(!cases.is_empty());

        let hook = OutgoingWebhook {
            trigger_words: Some(vec![
                "deploy".to_string(),
                "dep".to_string(),
                "status".to_string(),
            ]),
            ..Default::default()
        };
        let empty = OutgoingWebhook::default();

        for case in cases {
            let word = case["word"].as_str().unwrap();
            assert_eq!(
                hook.trigger_word_exact_match(word),
                case["exact"].as_bool().unwrap(),
                "TriggerWordExactMatch({word:?})"
            );
            assert_eq!(
                hook.trigger_word_starts_with(word),
                case["starts_with"].as_bool().unwrap(),
                "TriggerWordStartsWith({word:?})"
            );
            assert_eq!(
                hook.get_trigger_word(word, true),
                case["get_exact"].as_str().unwrap(),
                "GetTriggerWord({word:?}, exact)"
            );
            assert_eq!(
                hook.get_trigger_word(word, false),
                case["get_starts_with"].as_str().unwrap(),
                "GetTriggerWord({word:?}, prefix)"
            );
            assert_eq!(
                empty.trigger_word_starts_with(word),
                case["empty_trigger_set"].as_bool().unwrap(),
                "empty hook, {word:?}"
            );
        }
    }
}
