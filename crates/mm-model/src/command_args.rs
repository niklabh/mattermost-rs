//! Port of `model/command_args.go` — everything a slash-command handler is told about the
//! invocation.
//!
//! # Four of the eleven fields never reach the wire
//!
//! `SiteURL`, `T` (the i18n translate function), `UserMentions` and `ChannelMentions` all carry
//! `json:"-"`. The two mention maps are resolved server-side *after* the body is decoded, so a
//! `CommandArgs` that has round-tripped through JSON has empty maps — which is why
//! [`CommandArgs::add_user_mention`] lazily creates them rather than assuming a constructor ran.
//!
//! `T` has no counterpart here: i18n is a server concern and the translate function is not data.

use serde::{Deserialize, Serialize};

use crate::mention_map::{ChannelMentionMap, UserMentionMap};
use crate::serde_helpers::is_empty_str;

/// Port of `model.CommandArgs` (command_args.go:7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandArgs {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "root_id")]
    pub root_id: String,

    /// The pre-CRT name for the thread root. Both this and `root_id` are on the wire, and the
    /// server sets them to the same value.
    #[serde(rename = "parent_id")]
    pub parent_id: String,

    #[serde(rename = "trigger_id", skip_serializing_if = "is_empty_str")]
    pub trigger_id: String,

    #[serde(rename = "connection_id", skip_serializing_if = "is_empty_str")]
    pub connection_id: String,

    /// The raw command line, including the leading `/trigger`.
    #[serde(rename = "command")]
    pub command: String,

    /// `json:"-"`.
    #[serde(skip)]
    pub site_url: String,

    /// `json:"-"` — resolved server-side, username → user id.
    #[serde(skip)]
    pub user_mentions: UserMentionMap,

    /// `json:"-"` — resolved server-side, channel name → channel id.
    #[serde(skip)]
    pub channel_mentions: ChannelMentionMap,
}

impl CommandArgs {
    /// Port of `(*CommandArgs).AddUserMention` (command_args.go:32) — adds or **overrides**.
    pub fn add_user_mention(&mut self, username: impl Into<String>, user_id: impl Into<String>) {
        self.user_mentions.0.insert(username.into(), user_id.into());
    }

    /// Port of `(*CommandArgs).AddChannelMention` (command_args.go:42).
    pub fn add_channel_mention(
        &mut self,
        channel_name: impl Into<String>,
        channel_id: impl Into<String>,
    ) {
        self.channel_mentions
            .0
            .insert(channel_name.into(), channel_id.into());
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
    fn command_args_round_trips_the_fixture() {
        assert_fixture_round_trips!(CommandArgs, "command_args");
    }
}
