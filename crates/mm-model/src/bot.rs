//! Port of `server/public/model/bot.go`.
//!
//! A bot is "a special type of User meant for programmatic interactions", and its primary key
//! **is** the user id — `Bots.UserId` matches `Users.Id`. That is why `mm-store`'s user query
//! already LEFT JOINs this table to answer `is_bot`: the two rows describe one identity.
//!
//! Two upstream bugs are reproduced here rather than fixed, both confirmed against Go by
//! `fixtures/behaviour_bot.json`. See [`Bot::is_valid_create`] and [`BotList::etag`].

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::user::{User, normalize_email, normalize_username};
use crate::utils::{AppError, AppResult, etag, get_millis, is_valid_id};

/// Constants borrowed from files that are not yet ported. Same convention as
/// [`crate::user::external`]; each is pinned by the oracle so a drift upstream fails a test
/// rather than passing silently.
pub mod external {
    /// role.go:381
    pub const SYSTEM_USER_ROLE_ID: &str = "system_user";
    /// plugin_key_value.go:12 — the source of `BotCreatorIdMaxRunes`.
    pub const KEY_VALUE_PLUGIN_ID_MAX_RUNES: usize = 190;
}
use external::*;

/// `BotDisplayNameMaxRunes = UserFirstNameMaxRunes` (bot.go:14).
pub const BOT_DISPLAY_NAME_MAX_RUNES: usize = crate::user::USER_FIRST_NAME_MAX_RUNES;
/// bot.go:15
pub const BOT_DESCRIPTION_MAX_RUNES: usize = 1024;
/// `BotCreatorIdMaxRunes = KeyValuePluginIdMaxRunes` (bot.go:16) — "UserId or PluginId".
pub const BOT_CREATOR_ID_MAX_RUNES: usize = KEY_VALUE_PLUGIN_ID_MAX_RUNES;
/// bot.go:17
pub const BOT_WARN_METRIC_BOT_USERNAME: &str = "mattermost-advisor";
/// bot.go:18
pub const BOT_SYSTEM_BOT_USERNAME: &str = "system-bot";

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// Port of `model.Bot` (bot.go:24).
///
/// Three fields carry `omitempty` — `display_name`, `description`, `last_icon_update` — so a
/// zero-valued bot serialises six keys rather than nine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bot {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(
        rename = "display_name",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub display_name: String,

    #[serde(
        rename = "description",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,

    #[serde(rename = "owner_id")]
    pub owner_id: String,

    #[serde(rename = "last_icon_update", default, skip_serializing_if = "is_zero")]
    pub last_icon_update: i64,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,
}

/// Port of `model.BotPatch` (bot.go:51).
///
/// **No field carries `omitempty`**, so an all-nil patch is three explicit `null`s rather than
/// `{}` — the difference between "leave this alone" and "not mentioned", and it is on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotPatch {
    #[serde(rename = "username")]
    pub username: Option<String>,

    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "description")]
    pub description: Option<String>,
}

/// Port of `model.BotGetOptions` (bot.go:65). No `json:` tags in Go — a query-filter struct that
/// never reaches the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotGetOptions {
    pub owner_id: String,
    pub include_deleted: bool,
    pub only_orphaned: bool,
    pub page: i32,
    pub per_page: i32,
}

/// Port of `model.BotList` (bot.go:74).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BotList(pub Vec<Bot>);

impl std::ops::Deref for BotList {
    type Target = Vec<Bot>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Bot {
    /// Port of `(*Bot).Trace` (bot.go:77) — "the minimum information required to identify a bot
    /// for the purpose of logging".
    pub fn trace(&self) -> serde_json::Value {
        serde_json::json!({ "user_id": self.user_id })
    }

    /// Port of `(*Bot).IsValidCreate` (bot.go:86).
    ///
    /// Skips the fields `PreSave` fills in — `UserId`, `CreateAt`, `UpdateAt` are **not** checked
    /// here, which is the point of Go's "auto-filled on Create" comment.
    ///
    /// # The display-name branch reports the wrong error id, on purpose
    ///
    /// Go returns `model.bot.is_valid.user_id.app_error` for an over-long **display name**
    /// (bot.go:93). There is no `…display_name.app_error` anywhere in the tree — it is a
    /// copy-paste bug. Confirmed against Go: the oracle records `display_name_too_long` →
    /// `model.bot.is_valid.user_id.app_error` while `description_too_long` correctly reports
    /// `…description.app_error`.
    ///
    /// Reproduced rather than corrected, because a client that branches on the id would see a
    /// different answer from the two servers, and this one is reachable from any bot-creation
    /// form.
    pub fn is_valid_create(&self) -> AppResult {
        if !crate::user::is_valid_username(&self.username) {
            return Err(self.error("model.bot.is_valid.username.app_error"));
        }

        if self.display_name.chars().count() > BOT_DISPLAY_NAME_MAX_RUNES {
            // Not a typo here — a typo *there*. See the note above.
            return Err(self.error("model.bot.is_valid.user_id.app_error"));
        }

        if self.description.chars().count() > BOT_DESCRIPTION_MAX_RUNES {
            return Err(self.error("model.bot.is_valid.description.app_error"));
        }

        if self.owner_id.is_empty() || self.owner_id.chars().count() > BOT_CREATOR_ID_MAX_RUNES {
            return Err(self.error("model.bot.is_valid.creator_id.app_error"));
        }

        Ok(())
    }

    /// Port of `(*Bot).IsValid` (bot.go:110).
    ///
    /// Its own three checks run **first**, then it delegates to [`Bot::is_valid_create`]. The
    /// order is observable: a bot with both a zero `CreateAt` and a bad username reports
    /// `create_at`, not `username`.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.user_id) {
            return Err(self.error("model.bot.is_valid.user_id.app_error"));
        }

        if self.create_at == 0 {
            return Err(self.error("model.bot.is_valid.create_at.app_error"));
        }

        if self.update_at == 0 {
            return Err(self.error("model.bot.is_valid.update_at.app_error"));
        }

        self.is_valid_create()
    }

    /// Every validation failure in this file is built the same way: `Where` is the string
    /// `"Bot.IsValid"` even when raised from `IsValidCreate`, the params are `Trace()`, and the
    /// status is 400.
    fn error(&self, id: &str) -> Box<AppError> {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "user_id".to_owned(),
            serde_json::Value::String(self.user_id.clone()),
        );
        Box::new(AppError::new(
            "Bot.IsValid",
            id,
            Some(params),
            String::new(),
            400,
        ))
    }

    /// Port of `(*Bot).PreSave` (bot.go:124).
    ///
    /// Sets both timestamps to the same instant, **clears** `DeleteAt`, and normalises the
    /// username. Touches nothing else — notably not `OwnerId` or `Description`.
    pub fn pre_save(&mut self) {
        self.create_at = get_millis();
        self.update_at = self.create_at;
        self.delete_at = 0;
        self.username = normalize_username(&self.username);
    }

    /// Port of `(*Bot).PreUpdate` (bot.go:132).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*Bot).Etag` (bot.go:137).
    pub fn etag(&self) -> String {
        etag(&[&self.user_id, &self.update_at])
    }

    /// Port of `(*Bot).Patch` (bot.go:143).
    ///
    /// Go takes `*BotPatch` and dereferences it unconditionally, so a nil patch **panics**
    /// (confirmed by the oracle). Here the argument is a reference, which makes that state
    /// unrepresentable rather than reproducible — the one divergence in this file that is a
    /// consequence of the type system rather than a choice. See [D-095].
    pub fn patch(&mut self, patch: &BotPatch) {
        if let Some(username) = &patch.username {
            self.username = username.clone();
        }
        if let Some(display_name) = &patch.display_name {
            self.display_name = display_name.clone();
        }
        if let Some(description) = &patch.description {
            self.description = description.clone();
        }
    }

    /// Port of `(*Bot).WouldPatch` (bot.go:159).
    ///
    /// Unlike `Patch`, Go guards nil explicitly and answers `false`, so this one takes an
    /// `Option` — the nil case is reachable and has a defined answer.
    ///
    /// Note it compares against the current value: a patch setting a field to what it already
    /// holds would *apply* (a no-op) but reports `false` here.
    pub fn would_patch(&self, patch: Option<&BotPatch>) -> bool {
        let Some(patch) = patch else {
            return false;
        };
        if patch.username.as_ref().is_some_and(|v| *v != self.username) {
            return true;
        }
        if patch
            .display_name
            .as_ref()
            .is_some_and(|v| *v != self.display_name)
        {
            return true;
        }
        if patch
            .description
            .as_ref()
            .is_some_and(|v| *v != self.description)
        {
            return true;
        }
        false
    }

    /// Port of `(*Bot).Auditable` (bot.go:35).
    pub fn auditable(&self) -> serde_json::Value {
        serde_json::json!({
            "user_id": self.user_id,
            "username": self.username,
            "display_name": self.display_name,
            "description": self.description,
            "owner_id": self.owner_id,
            "last_icon_update": self.last_icon_update,
            "create_at": self.create_at,
            "update_at": self.update_at,
            "delete_at": self.delete_at,
        })
    }
}

impl BotPatch {
    /// Port of `(*BotPatch).Auditable` (bot.go:57). The pointers marshal as their pointed-to
    /// values, so a nil field becomes `null` rather than being dropped.
    pub fn auditable(&self) -> serde_json::Value {
        serde_json::json!({
            "username": self.username,
            "display_name": self.display_name,
            "description": self.description,
        })
    }
}

impl BotList {
    /// Port of `(*BotList).Etag` (bot.go:200).
    ///
    /// # The third component is always zero
    ///
    /// Go declares `var delta int64`, never assigns it, and passes it to `Etag` — so every bot
    /// list etag carries a literal `0` in that position, presumably a leftover from a version
    /// that computed something. Confirmed against Go for all six corpus cases.
    ///
    /// # `id` starts as the string "0", not empty
    ///
    /// So an empty list etags as `<version>.0.0.0.0`, and a list whose every `UpdateAt` is zero
    /// keeps `"0"` as the id too, because the comparison is strictly greater. Same trap as
    /// `Audits::etag` ([D-076]).
    pub fn etag(&self) -> String {
        let mut id = "0".to_owned();
        let mut t: i64 = 0;
        // Declared and never assigned in Go. Kept so the component count and value match.
        let delta: i64 = 0;

        for bot in &self.0 {
            if bot.update_at > t {
                t = bot.update_at;
                id = bot.user_id.clone();
            }
        }

        etag(&[&id, &t, &delta, &self.0.len()])
    }
}

/// Port of `UserFromBot` (bot.go:176).
///
/// The email is synthesised as `<username>@localhost` and then **normalised**, so a mixed-case
/// username yields a lower-case email while `Username` itself is copied verbatim — the two
/// disagree in case, which is Go's behaviour and is measured.
///
/// Everything not listed is the `User` zero value; `CreateAt`, `Nickname`, `Position` and the
/// rest are deliberately left unset.
pub fn user_from_bot(bot: &Bot) -> User {
    User {
        id: bot.user_id.clone(),
        username: bot.username.clone(),
        email: normalize_email(&format!("{}@localhost", bot.username)),
        first_name: bot.display_name.clone(),
        roles: SYSTEM_USER_ROLE_ID.to_owned(),
        ..Default::default()
    }
}

/// Port of `BotFromUser` (bot.go:187).
///
/// `DisplayName` comes from `GetDisplayName(ShowUsername)`, so it is the **username** — not the
/// first/last name and not the nickname. Confirmed against Go for a user carrying all three.
///
/// `OwnerId` and `UserId` are both the user's id, so a bot made this way owns itself.
pub fn bot_from_user(user: &User) -> Bot {
    Bot {
        owner_id: user.id.clone(),
        user_id: user.id.clone(),
        username: user.username.clone(),
        display_name: user.get_display_name(crate::user::external::SHOW_USERNAME),
        ..Default::default()
    }
}

/// Port of `MakeBotNotFoundError` (bot.go:214).
///
/// Go's comment is the reason this exists: *"The errors must the same in both cases to avoid
/// leaking that a user is a bot."* A missing bot and a forbidden one answer identically.
pub fn make_bot_not_found_error(where_: &str, user_id: &str) -> Box<AppError> {
    let mut params = std::collections::HashMap::new();
    params.insert(
        "user_id".to_owned(),
        serde_json::Value::String(user_id.to_owned()),
    );
    Box::new(AppError::new(
        where_,
        "store.sql_bot.get.missing.app_error",
        Some(params),
        String::new(),
        404,
    ))
}

/// Port of `IsBotDMChannel` (bot.go:220).
///
/// A DM channel's `Name` is the two member ids joined by `__`, so the bot is a participant when
/// its id is at either end. The separator is part of the test: a name that merely *contains* the
/// id matches neither branch.
pub fn is_bot_dm_channel(channel: &crate::channel::Channel, bot_user_id: &str) -> bool {
    if channel.channel_type != crate::channel::CHANNEL_TYPE_DIRECT {
        return false;
    }

    let mut prefix = String::with_capacity(bot_user_id.len() + 2);
    let _ = write!(prefix, "{bot_user_id}__");
    let mut suffix = String::with_capacity(bot_user_id.len() + 2);
    let _ = write!(suffix, "__{bot_user_id}");

    channel.name.starts_with(&prefix) || channel.name.ends_with(&suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialization parity against `fixtures/bot.json` — reflection-populated from Go, so every
    /// field carries a distinctive non-zero value and no `omitempty` can hide from its own test.
    #[test]
    fn bot_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/bot.json");
        let bot: Bot = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&bot).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn bot_patch_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/bot_patch.json");
        let patch: BotPatch = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&patch).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_bot.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::tests_support::*;
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_bot.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["BotDisplayNameMaxRunes"], BOT_DISPLAY_NAME_MAX_RUNES);
        assert_eq!(c["BotDescriptionMaxRunes"], BOT_DESCRIPTION_MAX_RUNES);
        assert_eq!(c["BotCreatorIdMaxRunes"], BOT_CREATOR_ID_MAX_RUNES);
        assert_eq!(c["BotWarnMetricBotUsername"], BOT_WARN_METRIC_BOT_USERNAME);
        assert_eq!(c["BotSystemBotUsername"], BOT_SYSTEM_BOT_USERNAME);
        // The two borrowed sources, so a drift upstream fails here rather than silently.
        assert_eq!(
            c["UserFirstNameMaxRunes"],
            crate::user::USER_FIRST_NAME_MAX_RUNES
        );
        assert_eq!(c["KeyValuePluginIdMaxRunes"], KEY_VALUE_PLUGIN_ID_MAX_RUNES);
        assert_eq!(c["SystemUserRoleId"], SYSTEM_USER_ROLE_ID);
    }

    #[test]
    fn wire_format_is_byte_exact() {
        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let bot: Bot = serde_json::from_str(expected).unwrap();
            let ours = serde_json::to_string(&bot).unwrap();
            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    #[test]
    fn is_valid_matches_go() {
        for case in oracle()["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bot = bot_for(name);
            check(name, bot.is_valid(), &case["result"]);
        }
    }

    /// Includes `display_name_too_long`, which Go answers with the **user_id** error id.
    #[test]
    fn is_valid_create_matches_go_including_the_copy_paste_bug() {
        let oracle = oracle();
        let cases = oracle["is_valid_create"].as_array().unwrap();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let bot = bot_for(name);
            check(name, bot.is_valid_create(), &case["result"]);
        }

        // Assert the bug explicitly, so deleting it from the port is a loud failure rather than
        // a quietly different error id.
        let bug = cases
            .iter()
            .find(|c| c["name"] == "display_name_too_long")
            .expect("the corpus covers it");
        assert_eq!(
            bug["result"]["id"], "model.bot.is_valid.user_id.app_error",
            "if Go ever fixes this, the port must follow rather than lead"
        );
    }

    #[test]
    fn etag_matches_go() {
        for case in oracle()["etag"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bot = bot_for(name);
            assert_eq!(
                bot.etag(),
                case["etag"].as_str().unwrap(),
                "etag mismatch for {name}"
            );
        }
    }

    #[test]
    fn list_etag_matches_go_including_the_unused_delta() {
        for case in oracle()["list_etag"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let list = bot_list_for(name);
            assert_eq!(list.len(), case["len"].as_u64().unwrap() as usize);
            assert_eq!(
                list.etag(),
                case["etag"].as_str().unwrap(),
                "list etag mismatch for {name}"
            );
        }
    }

    #[test]
    fn would_patch_matches_go() {
        for case in oracle()["would_patch"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let patch = patch_for(name);
            assert_eq!(
                valid_bot_local().would_patch(patch.as_ref()),
                case["would"].as_bool().unwrap(),
                "would_patch mismatch for {name}"
            );
        }
    }

    #[test]
    fn patch_matches_go() {
        for case in oracle()["patch"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            if case["panics"].as_bool().unwrap() {
                // Go panics on a nil patch; our signature makes that unrepresentable (D-095).
                assert_eq!(name, "nil_patch");
                continue;
            }
            let patch = patch_for(name).expect("non-nil");
            let mut bot = valid_bot_local();
            bot.patch(&patch);
            let ours = serde_json::to_string(&bot).unwrap();
            assert_eq!(
                ours,
                case["json"].as_str().unwrap(),
                "patch mismatch for {name}"
            );
        }
    }

    #[test]
    fn user_from_bot_matches_go() {
        for case in oracle()["user_from_bot"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bot = bot_for(name);
            let user = user_from_bot(&bot);
            assert_eq!(user.id, case["id"].as_str().unwrap(), "{name} id");
            assert_eq!(
                user.username,
                case["username"].as_str().unwrap(),
                "{name} username"
            );
            assert_eq!(user.email, case["email"].as_str().unwrap(), "{name} email");
            assert_eq!(
                user.first_name,
                case["first_name"].as_str().unwrap(),
                "{name} first_name"
            );
            assert_eq!(user.roles, case["roles"].as_str().unwrap(), "{name} roles");
            // The fields the conversion leaves alone.
            assert_eq!(user.last_name, case["last_name"].as_str().unwrap());
            assert_eq!(user.nickname, case["nickname"].as_str().unwrap());
            assert_eq!(user.create_at, case["create_at"].as_i64().unwrap());
        }
    }

    #[test]
    fn bot_from_user_matches_go() {
        for case in oracle()["bot_from_user"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let user = user_for(name);
            let bot = bot_from_user(&user);
            assert_eq!(
                bot.owner_id,
                case["owner_id"].as_str().unwrap(),
                "{name} owner_id"
            );
            assert_eq!(
                bot.user_id,
                case["user_id"].as_str().unwrap(),
                "{name} user_id"
            );
            assert_eq!(
                bot.username,
                case["username"].as_str().unwrap(),
                "{name} username"
            );
            assert_eq!(
                bot.display_name,
                case["display_name"].as_str().unwrap(),
                "{name} display_name — this is the USERNAME, not the full name"
            );
            assert_eq!(bot.create_at, case["create_at"].as_i64().unwrap());
        }
    }

    #[test]
    fn is_bot_dm_channel_matches_go() {
        for case in oracle()["is_bot_dm"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (channel, bot_id) = dm_case_for(name);
            assert_eq!(
                is_bot_dm_channel(&channel, &bot_id),
                case["result"].as_bool().unwrap(),
                "is_bot_dm mismatch for {name}"
            );
        }
    }

    #[test]
    fn not_found_error_matches_go() {
        let case = &oracle()["not_found_error"];
        let err = make_bot_not_found_error("SomeWhere", "y9i4er48tt8bukijy7i3u5y9ar");
        assert_eq!(err.id, case["id"].as_str().unwrap());
        assert_eq!(err.where_, case["where"].as_str().unwrap());
        assert_eq!(err.status_code, case["status"].as_i64().unwrap() as i32);
    }

    #[test]
    fn auditable_matches_go() {
        let oracle = oracle();
        let case = &oracle["auditable"];
        let bot = valid_bot_local();
        let expected: serde_json::Value =
            serde_json::from_str(case["bot"].as_str().unwrap()).unwrap();
        assert_eq!(bot.auditable(), expected);

        let expected_trace: serde_json::Value =
            serde_json::from_str(case["trace"].as_str().unwrap()).unwrap();
        assert_eq!(bot.trace(), expected_trace);
    }

    #[test]
    fn pre_save_and_pre_update_match_gos_invariants() {
        let oracle = oracle();
        let save = &oracle["pre_save"][0];
        let mut bot = valid_bot_local();
        bot.username = save["username_before"].as_str().unwrap().to_owned();
        bot.delete_at = save["delete_at_before"].as_i64().unwrap();
        let before = bot.clone();
        bot.pre_save();

        assert_eq!(
            bot.create_at, bot.update_at,
            "PreSave sets both to one instant"
        );
        assert_ne!(bot.create_at, 0);
        assert_eq!(bot.delete_at, 0, "PreSave clears DeleteAt");
        assert_eq!(bot.username, save["username_normalized"].as_str().unwrap());
        assert_eq!(bot.user_id, before.user_id);
        assert_eq!(bot.owner_id, before.owner_id);
        assert_eq!(bot.description, before.description);
        assert_eq!(bot.display_name, before.display_name);

        let mut updated = valid_bot_local();
        let create_at_before = updated.create_at;
        let delete_at_before = updated.delete_at;
        updated.pre_update();
        assert_ne!(updated.update_at, 0);
        assert_eq!(
            updated.create_at, create_at_before,
            "PreUpdate leaves CreateAt"
        );
        assert_eq!(updated.delete_at, delete_at_before);
    }
}

/// Case builders shared by the parity tests. Kept beside them so each corpus name has exactly one
/// definition, and a name present in the fixture but not here fails loudly.
#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn valid_bot_local() -> Bot {
        Bot {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            username: "botusername".to_owned(),
            display_name: "Bot Display Name".to_owned(),
            description: "a bot that does things".to_owned(),
            owner_id: "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            last_icon_update: 1_700_000_000_000,
            create_at: 1_600_000_000_000,
            update_at: 1_650_000_000_000,
            delete_at: 0,
        }
    }

    fn runes(n: usize) -> String {
        "a".repeat(n)
    }

    pub fn bot_for(name: &str) -> Bot {
        let mut b = valid_bot_local();
        match name {
            "valid" | "typical" => {}
            "zero" => b = Bot::default(),
            "empty_user_id" | "empty_user_id_is_fine_on_create" => b.user_id = String::new(),
            "short_user_id" => b.user_id = "abc".to_owned(),
            "zero_create_at" => b.create_at = 0,
            "zero_update_at" => b.update_at = 0,
            "zero_timestamps_are_fine_on_create" => {
                b.create_at = 0;
                b.update_at = 0;
            }
            "bad_username_via_is_valid" => b.username = "Bad Username!".to_owned(),
            "zero_create_at_and_bad_username" => {
                b.create_at = 0;
                b.username = "Bad Username!".to_owned();
            }
            "empty_username" => b.username = String::new(),
            "bad_username" => b.username = "Has Spaces".to_owned(),
            "display_name_too_long" => b.display_name = runes(BOT_DISPLAY_NAME_MAX_RUNES + 1),
            "display_name_at_limit" => b.display_name = runes(BOT_DISPLAY_NAME_MAX_RUNES),
            "display_name_at_limit_multibyte" => {
                b.display_name = "é".repeat(BOT_DISPLAY_NAME_MAX_RUNES)
            }
            "description_too_long" => b.description = runes(BOT_DESCRIPTION_MAX_RUNES + 1),
            "description_at_limit" => b.description = runes(BOT_DESCRIPTION_MAX_RUNES),
            "empty_owner_id" => b.owner_id = String::new(),
            "owner_id_too_long" => b.owner_id = runes(BOT_CREATOR_ID_MAX_RUNES + 1),
            "owner_id_at_limit" => b.owner_id = runes(BOT_CREATOR_ID_MAX_RUNES),
            "mixed_case_username" => b.username = "MixedCase".to_owned(),
            other => panic!("unmapped corpus case: {other}"),
        }
        b
    }

    pub fn bot_list_for(name: &str) -> BotList {
        let bot = |id: &str, update_at: i64| Bot {
            user_id: id.to_owned(),
            update_at,
            ..valid_bot_local()
        };
        const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
        match name {
            "empty" => BotList(vec![]),
            "one" => BotList(vec![bot("y9i4er48tt8bukijy7i3u5y9ar", 100)]),
            "ascending" => BotList(vec![bot(A, 100), bot(B, 200)]),
            "descending" => BotList(vec![bot(B, 200), bot(A, 100)]),
            "tie" => BotList(vec![bot(A, 200), bot(B, 200)]),
            "all_zero_update_at" => BotList(vec![bot(A, 0), bot(B, 0)]),
            other => panic!("unmapped list case: {other}"),
        }
    }

    pub fn patch_for(name: &str) -> Option<BotPatch> {
        let s = |v: &str| Some(v.to_owned());
        match name {
            "nil_patch" => None,
            "empty_patch" => Some(BotPatch::default()),
            "username_only" => Some(BotPatch {
                username: s("newname"),
                ..Default::default()
            }),
            "display_name_only" => Some(BotPatch {
                display_name: s("New Display"),
                ..Default::default()
            }),
            "description_only" => Some(BotPatch {
                description: s("new description"),
                ..Default::default()
            }),
            "all_three" => Some(BotPatch {
                username: s("n"),
                display_name: s("d"),
                description: s("x"),
            }),
            "same_username" => Some(BotPatch {
                username: s("botusername"),
                ..Default::default()
            }),
            "clear_display_name" => Some(BotPatch {
                display_name: s(""),
                ..Default::default()
            }),
            other => panic!("unmapped patch case: {other}"),
        }
    }

    pub fn user_for(name: &str) -> User {
        match name {
            "typical" => User {
                id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
                username: "someuser".to_owned(),
                first_name: "First".to_owned(),
                last_name: "Last".to_owned(),
                nickname: "Nick".to_owned(),
                ..Default::default()
            },
            "with_full_name" => User {
                id: "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                username: "uname".to_owned(),
                first_name: "Given".to_owned(),
                last_name: "Family".to_owned(),
                ..Default::default()
            },
            "empty" => User::default(),
            other => panic!("unmapped user case: {other}"),
        }
    }

    pub fn dm_case_for(name: &str) -> (crate::channel::Channel, String) {
        const BOT: &str = "y9i4er48tt8bukijy7i3u5y9ar";
        const OTHER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        let channel = |ty: &str, chname: String| crate::channel::Channel {
            channel_type: ty.to_owned(),
            name: chname,
            ..Default::default()
        };
        let c = match name {
            "direct_prefix" => channel(
                crate::channel::CHANNEL_TYPE_DIRECT,
                format!("{BOT}__{OTHER}"),
            ),
            "direct_suffix" => channel(
                crate::channel::CHANNEL_TYPE_DIRECT,
                format!("{OTHER}__{BOT}"),
            ),
            "direct_not_involved" => channel(
                crate::channel::CHANNEL_TYPE_DIRECT,
                format!("{OTHER}__{OTHER}"),
            ),
            "open_channel" => channel(crate::channel::CHANNEL_TYPE_OPEN, format!("{BOT}__{OTHER}")),
            "group_channel" => channel(
                crate::channel::CHANNEL_TYPE_GROUP,
                format!("{BOT}__{OTHER}"),
            ),
            "no_separator" => channel(crate::channel::CHANNEL_TYPE_DIRECT, BOT.to_owned()),
            "id_in_middle" => channel(
                crate::channel::CHANNEL_TYPE_DIRECT,
                format!("{OTHER}__{BOT}__x"),
            ),
            "empty_name" => channel(crate::channel::CHANNEL_TYPE_DIRECT, String::new()),
            other => panic!("unmapped dm case: {other}"),
        };
        (c, BOT.to_owned())
    }

    /// Compare an `AppResult` against the oracle's recorded answer.
    pub fn check(name: &str, got: AppResult, expected: &serde_json::Value) {
        if expected["ok"].as_bool().unwrap_or(false) {
            assert!(got.is_ok(), "{name}: expected ok, got {got:?}");
            return;
        }
        let err = got.expect_err(&format!("{name}: expected an error"));
        assert_eq!(err.id, expected["id"].as_str().unwrap(), "{name}: error id");
        assert_eq!(
            err.where_,
            expected["where"].as_str().unwrap(),
            "{name}: where"
        );
        assert_eq!(
            err.status_code,
            expected["status"].as_i64().unwrap() as i32,
            "{name}: status"
        );
    }
}
