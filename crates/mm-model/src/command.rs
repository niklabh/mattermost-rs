//! Port of `model/command.go` — a registered slash command.

use serde::{Deserialize, Serialize};

use crate::command_autocomplete::AutocompleteData;
use crate::manifest::is_valid_plugin_id;
use crate::serde_helpers::{is_empty_str, is_none};
use crate::utils::{
    AppError, AppResult, ID_LENGTH, get_millis, is_valid_http_url, is_valid_id, new_id,
};

/// Port of `model.CommandMethodPost` (command.go:12) — the single letter `P`, as stored.
pub const COMMAND_METHOD_POST: &str = "P";
/// Port of `model.CommandMethodGet` (command.go:13).
pub const COMMAND_METHOD_GET: &str = "G";
/// Port of `model.MinTriggerLength` (command.go:14).
pub const MIN_TRIGGER_LENGTH: usize = 1;
/// Port of `model.MaxTriggerLength` (command.go:15).
pub const MAX_TRIGGER_LENGTH: usize = 128;

/// The maximum `URL` length `IsValid` enforces. Inline in Go, named here so the two branches that
/// read it cannot drift.
pub const COMMAND_URL_MAX_LENGTH: usize = 1024;
/// The maximum `DisplayName` length.
pub const COMMAND_DISPLAY_NAME_MAX_LENGTH: usize = 64;
/// The maximum `Description` length.
pub const COMMAND_DESCRIPTION_MAX_LENGTH: usize = 128;

/// Port of `validCommandTriggerChars` (command.go:18) — `^[A-Za-z0-9_./-]+$`.
///
/// Inlined as a byte-set test rather than compiled: the pattern is a plain ASCII class anchored at
/// both ends, which is exactly "non-empty and every byte is in the set". Note the class **allows
/// `/` and `.`**, and a separate check then forbids `/` in position 0 only.
fn is_valid_command_trigger_chars(trigger: &str) -> bool {
    !trigger.is_empty()
        && trigger
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'/' || b == b'-')
}

/// Port of `model.Command` (command.go:20).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Command {
    #[serde(rename = "id")]
    pub id: String,

    /// The shared secret sent with each invocation. On the wire with no `omitempty` — it is
    /// [`Command::sanitize`] that removes it, not the tag.
    #[serde(rename = "token")]
    pub token: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// Empty for a plugin-created command — see [`Command::is_valid`].
    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// Without the leading `/`.
    #[serde(rename = "trigger")]
    pub trigger: String,

    /// [`COMMAND_METHOD_GET`] or [`COMMAND_METHOD_POST`].
    #[serde(rename = "method")]
    pub method: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    #[serde(rename = "auto_complete")]
    pub auto_complete: bool,

    #[serde(rename = "auto_complete_desc")]
    pub auto_complete_desc: String,

    #[serde(rename = "auto_complete_hint")]
    pub auto_complete_hint: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "url")]
    pub url: String,

    /// The plugin that registered this command; blank when a user did.
    #[serde(rename = "plugin_id")]
    pub plugin_id: String,

    /// `db:"-"` — computed, never stored.
    #[serde(rename = "autocomplete_data", skip_serializing_if = "is_none")]
    pub autocomplete_data: Option<AutocompleteData>,

    /// A base64-encoded SVG. `db:"-"`.
    #[serde(
        rename = "autocomplete_icon_data",
        skip_serializing_if = "is_empty_str"
    )]
    pub autocomplete_icon_data: String,
}

impl Command {
    /// Port of `(*Command).IsValid` (command.go:68).
    ///
    /// The creator/plugin rules are the subtle part and they are **three** separate branches, not
    /// one exclusive-or:
    ///
    /// 1. no `creator_id` requires a valid `plugin_id`;
    /// 2. no `plugin_id` requires a valid `creator_id`;
    /// 3. having **both** is rejected, with the same error id as (1) but a detail string.
    ///
    /// So a command with neither fails at (1), and the id it reports is `plugin_id`.
    ///
    /// The trigger check measures **bytes** (`len`), forbids `/` only at index 0, and applies the
    /// character class to the whole string.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if self.token.len() != ID_LENGTH {
            return Err(err("token", String::new()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", String::new()));
        }

        if self.update_at == 0 {
            return Err(err("update_at", String::new()));
        }

        // A blank CreatorId means this should be a plugin-created command.
        if self.creator_id.is_empty() && !is_valid_plugin_id(&self.plugin_id) {
            return Err(err("plugin_id", String::new()));
        }

        // A blank PluginId means this should be associated with a user.
        if self.plugin_id.is_empty() && !is_valid_id(&self.creator_id) {
            return Err(err("user_id", String::new()));
        }

        if !self.creator_id.is_empty() && !self.plugin_id.is_empty() {
            return Err(err(
                "plugin_id",
                "command cannot have both a CreatorId and a PluginId".to_string(),
            ));
        }

        if !is_valid_id(&self.team_id) {
            return Err(err("team_id", String::new()));
        }

        if self.trigger.len() < MIN_TRIGGER_LENGTH
            || self.trigger.len() > MAX_TRIGGER_LENGTH
            || self.trigger.starts_with('/')
            || !is_valid_command_trigger_chars(&self.trigger)
        {
            return Err(err("trigger", String::new()));
        }

        if self.url.is_empty() || self.url.len() > COMMAND_URL_MAX_LENGTH {
            return Err(err("url", String::new()));
        }

        if !is_valid_http_url(&self.url) {
            return Err(err("url_http", String::new()));
        }

        if !(self.method == COMMAND_METHOD_GET || self.method == COMMAND_METHOD_POST) {
            return Err(err("method", String::new()));
        }

        if self.display_name.len() > COMMAND_DISPLAY_NAME_MAX_LENGTH {
            return Err(err("display_name", String::new()));
        }

        if self.description.len() > COMMAND_DESCRIPTION_MAX_LENGTH {
            return Err(err("description", String::new()));
        }

        if let Some(autocomplete_data) = &self.autocomplete_data {
            if let Err(inner) = autocomplete_data.is_valid() {
                return Err(Box::new(
                    AppError::new(
                        "Command.IsValid",
                        "model.command.is_valid.autocomplete_data.app_error",
                        None,
                        "",
                        400,
                    )
                    .wrap(inner),
                ));
            }
        }

        Ok(())
    }

    /// Port of `(*Command).PreSave` (command.go:133).
    ///
    /// `create_at` is set **unconditionally**, like `terms_of_service.go` and unlike most of the
    /// package: re-saving rewrites the creation time.
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

    /// Port of `(*Command).PreUpdate` (command.go:145).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*Command).Sanitize` (command.go:149) — what a non-owner is allowed to see.
    ///
    /// Six fields cleared. Note `url`, `method` and `username` go too, not just the token: the
    /// endpoint a command posts to is itself a secret.
    pub fn sanitize(&mut self) {
        self.token.clear();
        self.creator_id.clear();
        self.method.clear();
        self.url.clear();
        self.username.clear();
        self.icon_url.clear();
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "Command.IsValid",
        format!("model.command.is_valid.{field}.app_error"),
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
    fn command_round_trips_the_fixture() {
        assert_fixture_round_trips!(Command, "command");
    }
}
