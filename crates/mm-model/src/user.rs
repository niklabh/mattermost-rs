//! Port of `model/user.go` (user.go:1–1160).
//!
//! # Deliberately not translated here
//!
//! - **`User::IsValid`** (user.go:383) and **`PreSave`** (user.go:486). Both depend on ports
//!   that are parser work in their own right — `IsValidEmail` (Go's `net/mail`) and
//!   `IsValidLocale` (`golang.org/x/text/language`, BCP 47) — plus `CustomStatus` and
//!   `timezones.DefaultUserTimezone()`. Shipping them against a guessed email or locale rule
//!   would put a wrong validator on the write path, which is worse than not having one.
//! - `IsValidUserRoles` needs `IsValidRoleName` from `role.go`.
//! - `CleanUsername` takes an mlog logger; it belongs with the logging layer.
//! - `GetTimezoneLocation` needs a tz database lookup (`chrono-tz`), deferred with `IsValid`.
//! - `Auditable`/`LogClone` are audit-log projections, not wire types; they follow the audit
//!   layer.
//! - `UserSlice` filters (user.go:295–362) are `Iterator::filter` at every call site.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::custom_status::{CustomStatus, CustomStatusError};
use crate::utils::{
    self, StringArray, StringMap, etag, get_millis, get_preferred_timezone, new_id, new_username,
    sanitize_unicode,
};

// ---------------------------------------------------------------------------
// Constants (user.go:25-72)
// ---------------------------------------------------------------------------

pub const ME: &str = "me";

pub const USER_NOTIFY_ALL: &str = "all";
pub const USER_NOTIFY_HERE: &str = "here";
pub const USER_NOTIFY_MENTION: &str = "mention";
pub const USER_NOTIFY_NONE: &str = "none";

pub const DESKTOP_NOTIFY_PROP: &str = "desktop";
pub const DESKTOP_SOUND_NOTIFY_PROP: &str = "desktop_sound";
pub const MARK_UNREAD_NOTIFY_PROP: &str = "mark_unread";
pub const PUSH_NOTIFY_PROP: &str = "push";
pub const PUSH_STATUS_NOTIFY_PROP: &str = "push_status";
pub const EMAIL_NOTIFY_PROP: &str = "email";
pub const CHANNEL_MENTIONS_NOTIFY_PROP: &str = "channel";
pub const COMMENTS_NOTIFY_PROP: &str = "comments";
pub const MENTION_KEYS_NOTIFY_PROP: &str = "mention_keys";
pub const HIGHLIGHTS_NOTIFY_PROP: &str = "highlight_keys";
pub const COMMENTS_NOTIFY_NEVER: &str = "never";
pub const COMMENTS_NOTIFY_ROOT: &str = "root";
pub const COMMENTS_NOTIFY_ANY: &str = "any";
pub const COMMENTS_NOTIFY_CRT: &str = "crt";
pub const FIRST_NAME_NOTIFY_PROP: &str = "first_name";
pub const AUTO_RESPONDER_ACTIVE_NOTIFY_PROP: &str = "auto_responder_active";
pub const AUTO_RESPONDER_MESSAGE_NOTIFY_PROP: &str = "auto_responder_message";
pub const DESKTOP_THREADS_NOTIFY_PROP: &str = "desktop_threads";
pub const PUSH_THREADS_NOTIFY_PROP: &str = "push_threads";
pub const EMAIL_THREADS_NOTIFY_PROP: &str = "email_threads";
pub const CHANNEL_MENTION_AUTO_FOLLOW_THREADS_PROP: &str = "channel_mention_auto_follow_threads";

pub const DEFAULT_LOCALE: &str = "en";
pub const USER_AUTH_SERVICE_EMAIL: &str = "email";
pub const USER_AUTH_SERVICE_MAGIC_LINK: &str = "magic_link";

pub const USER_EMAIL_MAX_LENGTH: usize = 128;
pub const USER_NICKNAME_MAX_RUNES: usize = 64;
pub const USER_POSITION_MAX_RUNES: usize = 128;
pub const USER_FIRST_NAME_MAX_RUNES: usize = 64;
pub const USER_LAST_NAME_MAX_RUNES: usize = 64;
pub const USER_AUTH_DATA_MAX_LENGTH: usize = 128;
pub const USER_NAME_MAX_LENGTH: usize = 64;
pub const USER_NAME_MIN_LENGTH: usize = 1;
pub const USER_PASSWORD_MAX_LENGTH: usize = 72;
pub const USER_LOCALE_MAX_LENGTH: usize = 5;
pub const USER_TIMEZONE_MAX_RUNES: usize = 256;
pub const USER_ROLES_MAX_LENGTH: usize = 256;

/// Constants owned by other Go files, duplicated here because `user.go` needs them.
///
/// Each moves to its own module when that file is translated; the ledger records the debt.
pub mod external {
    /// role.go:380
    pub const SYSTEM_GUEST_ROLE_ID: &str = "system_guest";
    /// role.go:382
    pub const SYSTEM_ADMIN_ROLE_ID: &str = "system_admin";
    /// ldap.go:7
    pub const USER_AUTH_SERVICE_LDAP: &str = "ldap";
    /// saml.go:12
    pub const USER_AUTH_SERVICE_SAML: &str = "saml";
    /// config.go:67-71
    pub const SERVICE_GITLAB: &str = "gitlab";
    pub const SERVICE_GOOGLE: &str = "google";
    pub const SERVICE_OFFICE365: &str = "office365";
    pub const SERVICE_OPENID: &str = "openid";
    /// config.go:83-85
    pub const SHOW_USERNAME: &str = "username";
    pub const SHOW_NICKNAME_FULL_NAME: &str = "nickname_full_name";
    pub const SHOW_FULL_NAME: &str = "full_name";
    /// custom_status.go:14 — now owned by [`crate::custom_status`], re-exported so the
    /// `external::` path keeps working. No longer a borrow: there is one definition.
    pub use crate::custom_status::USER_PROPS_KEY_CUSTOM_STATUS;
    /// shared_channel.go:18-20
    pub const USER_PROPS_KEY_REMOTE_EMAIL: &str = "RemoteEmail";
    pub const USER_PROPS_KEY_ORIGINAL_REMOTE_ID: &str = "OriginalRemoteId";
    pub const USER_ORIGINAL_REMOTE_ID_UNKNOWN: &str = "UNKNOWN";
    /// status.go:16 — now owned by [`crate::status`], re-exported so the `external::` path
    /// keeps working. No longer a borrow: there is one definition.
    pub use crate::status::STATUS_ONLINE;
}

use external::*;

// ---------------------------------------------------------------------------
// serde skip predicates — one per Go `omitempty` shape
// ---------------------------------------------------------------------------

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Go's `omitempty` on a map omits both nil **and** empty, unlike `Option::is_none`.
fn map_is_empty(m: &Option<StringMap>) -> bool {
    m.as_ref().is_none_or(StringMap::is_empty)
}

/// Same for slices.
fn slice_is_empty(v: &Option<StringArray>) -> bool {
    v.as_ref().is_none_or(Vec::is_empty)
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// Port of `model.User` (user.go:87).
///
/// Three field shapes here are easy to get wrong and all three are load-bearing:
///
/// - `props`/`notify_props` carry `omitempty`, so nil **and** empty are omitted — but the nil
///   vs empty distinction is meaningful internally (`MakeNonNil`, `GetOriginalRemoteID` test
///   for nil), hence `Option` plus an emptiness predicate rather than a bare map.
/// - `timezone` has **no** `omitempty`, so a nil map serialises as `null`, not `{}`. It is
///   `Option` with no skip predicate for exactly that reason.
/// - `auth_data` is `*string` with `omitempty`: `Some("")` serialises as `""` and only `None`
///   is omitted. `Sanitize` relies on this — it sets a pointer to the empty string, which
///   must still appear on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at", default, skip_serializing_if = "is_zero")]
    pub create_at: i64,

    #[serde(rename = "update_at", default, skip_serializing_if = "is_zero")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "password", default, skip_serializing_if = "String::is_empty")]
    pub password: String,

    #[serde(rename = "auth_data", default, skip_serializing_if = "Option::is_none")]
    pub auth_data: Option<String>,

    #[serde(rename = "auth_service")]
    pub auth_service: String,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "email_verified", default, skip_serializing_if = "is_false")]
    pub email_verified: bool,

    #[serde(rename = "nickname")]
    pub nickname: String,

    #[serde(rename = "first_name")]
    pub first_name: String,

    #[serde(rename = "last_name")]
    pub last_name: String,

    #[serde(rename = "position")]
    pub position: String,

    #[serde(rename = "roles")]
    pub roles: String,

    #[serde(rename = "allow_marketing", default, skip_serializing_if = "is_false")]
    pub allow_marketing: bool,

    #[serde(rename = "props", default, skip_serializing_if = "map_is_empty")]
    pub props: Option<StringMap>,

    #[serde(rename = "notify_props", default, skip_serializing_if = "map_is_empty")]
    pub notify_props: Option<StringMap>,

    #[serde(
        rename = "last_password_update",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub last_password_update: i64,

    #[serde(
        rename = "last_picture_update",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub last_picture_update: i64,

    /// Go `int`, which is 64-bit on every platform Mattermost ships.
    #[serde(rename = "failed_attempts", default, skip_serializing_if = "is_zero")]
    pub failed_attempts: i64,

    #[serde(rename = "locale")]
    pub locale: String,

    /// No `omitempty` in Go — a nil map must serialise as `null`.
    #[serde(rename = "timezone")]
    pub timezone: Option<StringMap>,

    #[serde(rename = "mfa_active", default, skip_serializing_if = "is_false")]
    pub mfa_active: bool,

    #[serde(
        rename = "mfa_secret",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub mfa_secret: String,

    #[serde(rename = "remote_id", default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,

    #[serde(rename = "last_activity_at", default, skip_serializing_if = "is_zero")]
    pub last_activity_at: i64,

    #[serde(rename = "is_bot", default, skip_serializing_if = "is_false")]
    pub is_bot: bool,

    #[serde(
        rename = "bot_description",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub bot_description: String,

    #[serde(
        rename = "bot_last_icon_update",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub bot_last_icon_update: i64,

    #[serde(
        rename = "terms_of_service_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub terms_of_service_id: String,

    #[serde(
        rename = "terms_of_service_create_at",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub terms_of_service_create_at: i64,

    #[serde(rename = "disable_welcome_email")]
    pub disable_welcome_email: bool,

    #[serde(rename = "last_login", default, skip_serializing_if = "is_zero")]
    pub last_login: i64,

    #[serde(
        rename = "mfa_used_timestamps",
        default,
        skip_serializing_if = "slice_is_empty"
    )]
    pub mfa_used_timestamps: Option<StringArray>,
}

/// Port of `model.UserPatch` (user.go:193).
///
/// Every `*string` here is `Option<String>` with **no** skip predicate except `password`,
/// matching Go: only `password` and the two maps carry `omitempty`, so a patch that clears a
/// field sends an explicit `null` and one that omits it sends nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPatch {
    #[serde(rename = "username")]
    pub username: Option<String>,

    #[serde(rename = "password", default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    #[serde(rename = "nickname")]
    pub nickname: Option<String>,

    #[serde(rename = "first_name")]
    pub first_name: Option<String>,

    #[serde(rename = "last_name")]
    pub last_name: Option<String>,

    #[serde(rename = "position")]
    pub position: Option<String>,

    #[serde(rename = "email")]
    pub email: Option<String>,

    #[serde(rename = "props", default, skip_serializing_if = "map_is_empty")]
    pub props: Option<StringMap>,

    #[serde(rename = "notify_props", default, skip_serializing_if = "map_is_empty")]
    pub notify_props: Option<StringMap>,

    #[serde(rename = "locale")]
    pub locale: Option<String>,

    #[serde(rename = "timezone")]
    pub timezone: Option<StringMap>,

    #[serde(rename = "remote_id")]
    pub remote_id: Option<String>,
}

/// Port of `model.UserAuth` (user.go:225).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAuth {
    #[serde(rename = "auth_data", default, skip_serializing_if = "Option::is_none")]
    pub auth_data: Option<String>,

    #[serde(
        rename = "auth_service",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub auth_service: String,
}

impl UserAuth {
    /// Port of `(*UserAuth).IsValid` (user.go:236).
    ///
    /// Email auth requires **no** auth data; every other service requires non-empty data.
    pub fn is_valid(&self) -> bool {
        if !is_valid_user_auth_service(&self.auth_service) {
            return false;
        }
        if self.auth_service == USER_AUTH_SERVICE_EMAIL {
            return self.auth_data.is_none();
        }
        self.auth_data.as_ref().is_some_and(|d| !d.is_empty())
    }
}

/// Port of `model.UserForIndexing` (user.go:249).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserForIndexing {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "username")]
    pub username: String,
    #[serde(rename = "nickname")]
    pub nickname: String,
    #[serde(rename = "first_name")]
    pub first_name: String,
    #[serde(rename = "last_name")]
    pub last_name: String,
    #[serde(rename = "roles")]
    pub roles: String,
    #[serde(rename = "create_at")]
    pub create_at: i64,
    #[serde(rename = "delete_at")]
    pub delete_at: i64,
    /// Note the tag: the Go field is `TeamsIds` but the JSON key is singular `team_id`.
    #[serde(rename = "team_id")]
    pub teams_ids: Vec<String>,
    /// Likewise `ChannelsIds` -> `channel_id`.
    #[serde(rename = "channel_id")]
    pub channels_ids: Vec<String>,
}

impl User {
    // -- identity / roles ---------------------------------------------------

    /// Port of `(*User).GetRoles` (user.go:866). Go's `strings.Fields` splits on runs of any
    /// whitespace, not single spaces.
    pub fn get_roles(&self) -> Vec<&str> {
        self.roles.split_whitespace().collect()
    }

    /// Port of `(*User).GetRawRoles` (user.go:870).
    pub fn get_raw_roles(&self) -> &str {
        &self.roles
    }

    /// Port of `(*User).Etag` (user.go:691).
    ///
    /// The two display flags are part of the key because they change what `Sanitize` leaves on
    /// the wire, so the same user rendered for two viewers must not share a cache entry.
    pub fn etag(&self, show_full_name: bool, show_email: bool) -> String {
        etag(&[
            &self.id,
            &self.update_at,
            &self.terms_of_service_id,
            &self.terms_of_service_create_at,
            &show_full_name,
            &show_email,
            &self.bot_last_icon_update,
        ])
    }

    /// Port of `(*User).IsInRole` (user.go:908).
    pub fn is_in_role(&self, in_role: &str) -> bool {
        is_in_role(&self.roles, in_role)
    }

    /// Port of `(*User).IsGuest` (user.go:893).
    pub fn is_guest(&self) -> bool {
        is_in_role(&self.roles, SYSTEM_GUEST_ROLE_ID)
    }

    /// Port of `(*User).IsSystemAdmin` (user.go:902).
    pub fn is_system_admin(&self) -> bool {
        is_in_role(&self.roles, SYSTEM_ADMIN_ROLE_ID)
    }

    /// Port of `(*User).IsMagicLinkEnabled` (user.go:897). Guests only.
    pub fn is_magic_link_enabled(&self) -> bool {
        self.auth_service == USER_AUTH_SERVICE_MAGIC_LINK && self.is_guest()
    }

    /// Port of `(*User).IsSSOUser` (user.go:920).
    pub fn is_sso_user(&self) -> bool {
        !self.auth_service.is_empty() && self.auth_service != USER_AUTH_SERVICE_EMAIL
    }

    /// Port of `(*User).IsOAuthUser` (user.go:924).
    pub fn is_oauth_user(&self) -> bool {
        matches!(
            self.auth_service.as_str(),
            SERVICE_GITLAB | SERVICE_GOOGLE | SERVICE_OFFICE365 | SERVICE_OPENID
        )
    }

    /// Port of `(*User).IsLDAPUser` (user.go:931).
    pub fn is_ldap_user(&self) -> bool {
        self.auth_service == USER_AUTH_SERVICE_LDAP
    }

    /// Port of `(*User).IsSAMLUser` (user.go:935).
    pub fn is_saml_user(&self) -> bool {
        self.auth_service == USER_AUTH_SERVICE_SAML
    }

    // -- remote / auth data -------------------------------------------------

    /// Port of `(*User).IsRemote` (user.go:969).
    pub fn is_remote(&self) -> bool {
        !self.get_remote_id().is_empty()
    }

    /// Port of `(*User).GetRemoteID` (user.go:974). `SafeDereference` on a nil pointer is `""`.
    pub fn get_remote_id(&self) -> &str {
        self.remote_id.as_deref().unwrap_or_default()
    }

    /// Port of `(*User).GetAuthData` (user.go:994).
    pub fn get_auth_data(&self) -> &str {
        self.auth_data.as_deref().unwrap_or_default()
    }

    /// Port of `(*User).GetOriginalRemoteID` (user.go:978).
    pub fn get_original_remote_id(&self) -> &str {
        match &self.props {
            None => {
                if self.is_remote() {
                    USER_ORIGINAL_REMOTE_ID_UNKNOWN
                } else {
                    ""
                }
            }
            Some(props) => match props.get(USER_PROPS_KEY_ORIGINAL_REMOTE_ID) {
                Some(id) if !id.is_empty() => id,
                _ if self.is_remote() => USER_ORIGINAL_REMOTE_ID_UNKNOWN,
                _ => "",
            },
        }
    }

    // -- props --------------------------------------------------------------

    /// Port of `(*User).GetProp` (user.go:999).
    pub fn get_prop(&self, name: &str) -> Option<&str> {
        self.props.as_ref()?.get(name).map(String::as_str)
    }

    /// Port of `(*User).SetProp` (user.go:1006). Creates the map when nil.
    pub fn set_prop(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.props
            .get_or_insert_with(StringMap::new)
            .insert(name.into(), value.into());
    }

    /// Port of `(*User).MakeNonNil` (user.go:765).
    pub fn make_non_nil(&mut self) {
        self.props.get_or_insert_with(StringMap::new);
        self.notify_props.get_or_insert_with(StringMap::new);
    }

    /// Port of `(*User).AddNotifyProp` (user.go:775).
    pub fn add_notify_prop(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.make_non_nil();
        if let Some(props) = self.notify_props.as_mut() {
            props.insert(key.into(), value.into());
        }
    }

    /// Port of `(*User).SetCustomStatus` (user.go:781).
    ///
    /// Stores the status as a **marshalled string** under `props["customStatus"]`, so the
    /// bytes matter — see [`CustomStatus::marshal`], which applies Go's HTML escaping.
    ///
    /// Go marshals the *pointer*, so `SetCustomStatus(nil)` is not an error and not a no-op: it
    /// writes the four bytes `null`. That branch is unrepresentable here.
    pub fn set_custom_status(&mut self, cs: &CustomStatus) -> Result<(), CustomStatusError> {
        self.make_non_nil();
        let encoded = cs.marshal()?;
        self.set_prop(USER_PROPS_KEY_CUSTOM_STATUS, encoded);
        Ok(())
    }

    /// Port of `(*User).GetCustomStatus` (user.go:791).
    ///
    /// **The unmarshal error is discarded** (`_ = json.Unmarshal(...)`), which is what makes
    /// this subtle. Go returns a non-nil status far more often than it looks: `{}` decodes to a
    /// zero status, missing keys are zero-filled, and even `"a string"`, `0`, `true` and `[]`
    /// come back non-nil, because the decoder allocates the pointer before it discovers the
    /// value is not an object. Only a *syntax* error, an absent key, an empty string and the
    /// literal `null` yield nil.
    ///
    /// One case is not reproduced exactly: Go's decoder is not all-or-nothing, so a **type**
    /// error (`{"emoji":123,"text":"kept"}`) leaves the successfully decoded fields in place.
    /// This returns a zero status there instead of a partially populated one — the non-nil-ness
    /// matches, the field values do not. See D-026; `validate_custom_status` below, which is
    /// the only caller whose answer reaches the wire, is exact.
    pub fn get_custom_status(&self) -> Option<CustomStatus> {
        let data = self.get_prop(USER_PROPS_KEY_CUSTOM_STATUS)?;

        // Go's two-stage behaviour: nothing is written for a malformed document, but a
        // well-formed one that simply is not a CustomStatus still allocates the value.
        let value: serde_json::Value = serde_json::from_str(data).ok()?;
        if value.is_null() {
            return None;
        }
        Some(serde_json::from_value(value).unwrap_or_default())
    }

    /// Port of `(*User).CustomStatus` (user.go:799).
    ///
    /// Byte-for-byte identical to `GetCustomStatus` in the Go source — two names for one
    /// function. Kept so call-site ports stay mechanical.
    pub fn custom_status(&self) -> Option<CustomStatus> {
        self.get_custom_status()
    }

    /// Port of `(*User).ClearCustomStatus` (user.go:809).
    ///
    /// Writes an empty string rather than removing the key, so the prop still exists
    /// afterwards. `validate_custom_status` treats that as valid.
    pub fn clear_custom_status(&mut self) {
        self.make_non_nil();
        self.set_prop(USER_PROPS_KEY_CUSTOM_STATUS, "");
    }

    /// Port of `(*User).ValidateCustomStatus` (user.go:814).
    ///
    /// True unless the prop is present, non-empty, and decodes to nothing. Go expresses that as
    /// "`GetCustomStatus()` returned nil", which reduces to a much narrower test than a full
    /// decode: the prop must be **syntactically valid JSON and not `null`**. A type error, a
    /// bare string, a number and `{}` all pass, because Go still produces a value for them.
    ///
    /// Written against that predicate rather than against `get_custom_status` so the
    /// partial-decode divergence in D-026 cannot leak into `User::is_valid`, which is where
    /// this answer ends up.
    pub fn validate_custom_status(&self) -> bool {
        let Some(status) = self.get_prop(USER_PROPS_KEY_CUSTOM_STATUS) else {
            return true;
        };
        if status.is_empty() {
            return true;
        }
        serde_json::from_str::<serde_json::Value>(status).is_ok_and(|v| !v.is_null())
    }

    /// Port of `(*User).SetDefaultNotifications` (user.go:597). Replaces the map wholesale.
    pub fn set_default_notifications(&mut self) {
        let mut props = StringMap::new();
        props.insert(EMAIL_NOTIFY_PROP.into(), "true".into());
        props.insert(PUSH_NOTIFY_PROP.into(), USER_NOTIFY_MENTION.into());
        props.insert(DESKTOP_NOTIFY_PROP.into(), USER_NOTIFY_MENTION.into());
        props.insert(DESKTOP_SOUND_NOTIFY_PROP.into(), "true".into());
        props.insert(MENTION_KEYS_NOTIFY_PROP.into(), String::new());
        props.insert(CHANNEL_MENTIONS_NOTIFY_PROP.into(), "true".into());
        props.insert(PUSH_STATUS_NOTIFY_PROP.into(), STATUS_ONLINE.into());
        props.insert(COMMENTS_NOTIFY_PROP.into(), COMMENTS_NOTIFY_NEVER.into());
        props.insert(FIRST_NAME_NOTIFY_PROP.into(), "false".into());
        props.insert(DESKTOP_THREADS_NOTIFY_PROP.into(), USER_NOTIFY_ALL.into());
        props.insert(EMAIL_THREADS_NOTIFY_PROP.into(), USER_NOTIFY_ALL.into());
        props.insert(PUSH_THREADS_NOTIFY_PROP.into(), USER_NOTIFY_ALL.into());
        props.insert(
            CHANNEL_MENTION_AUTO_FOLLOW_THREADS_PROP.into(),
            "true".into(),
        );
        self.notify_props = Some(props);
    }

    /// Port of `(*User).GetMentionKeys` (user.go:628). Splits on `,`, trims, drops blanks.
    pub fn get_mention_keys(&self) -> Vec<String> {
        let raw = self
            .notify_props
            .as_ref()
            .and_then(|p| p.get(MENTION_KEYS_NOTIFY_PROP))
            .map_or("", String::as_str);

        raw.split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Port of `(*User).UpdateMentionKeysFromUsername` (user.go:614).
    ///
    /// Note the leading comma Go produces: when any keys survive, the value becomes
    /// `",key1,key2"` — an empty string concatenated with `"," + join(...)`. Reproduced
    /// deliberately; clients tolerate the blank first element.
    pub fn update_mention_keys_from_username(&mut self, old_username: &str) {
        let at_old = format!("@{old_username}");
        let kept: Vec<String> = self
            .get_mention_keys()
            .into_iter()
            .filter(|k| k != old_username && *k != at_old)
            .collect();

        let value = if kept.is_empty() {
            String::new()
        } else {
            format!(",{}", kept.join(","))
        };
        self.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, value);
    }

    // -- names --------------------------------------------------------------

    /// Port of `(*User).GetFullName` (user.go:825).
    pub fn get_full_name(&self) -> String {
        match (self.first_name.is_empty(), self.last_name.is_empty()) {
            (false, false) => format!("{} {}", self.first_name, self.last_name),
            (false, true) => self.first_name.clone(),
            (true, false) => self.last_name.clone(),
            (true, true) => String::new(),
        }
    }

    /// Port of `(*User).getDisplayName` (user.go:836).
    fn display_name_from(&self, base_name: &str, name_format: &str) -> String {
        if name_format == SHOW_NICKNAME_FULL_NAME {
            if !self.nickname.is_empty() {
                return self.nickname.clone();
            }
            let full = self.get_full_name();
            if !full.is_empty() {
                return full;
            }
        } else if name_format == SHOW_FULL_NAME {
            let full = self.get_full_name();
            if !full.is_empty() {
                return full;
            }
        }
        base_name.to_string()
    }

    /// Port of `(*User).GetDisplayName` (user.go:854).
    pub fn get_display_name(&self, name_format: &str) -> String {
        self.display_name_from(&self.username, name_format)
    }

    /// Port of `(*User).GetDisplayNameWithPrefix` (user.go:860).
    pub fn get_display_name_with_prefix(&self, name_format: &str, prefix: &str) -> String {
        self.display_name_from(&format!("{prefix}{}", self.username), name_format)
    }

    /// Port of `(*User).GetPreferredTimezone` (user.go:956).
    pub fn get_preferred_timezone(&self) -> &str {
        static EMPTY: LazyLock<StringMap> = LazyLock::new(StringMap::new);
        get_preferred_timezone(self.timezone.as_ref().unwrap_or(&EMPTY))
    }

    /// Port of `(*User).EmailDomain` (user.go:1144).
    pub fn email_domain(&self) -> &str {
        self.email.rsplit_once('@').map_or("", |(_, domain)| domain)
    }

    // -- mutation -----------------------------------------------------------

    /// Port of `(*User).Patch` (user.go:644). Only non-nil patch fields are applied.
    pub fn patch(&mut self, patch: &UserPatch) {
        if let Some(v) = &patch.username {
            self.username = v.clone();
        }
        if let Some(v) = &patch.nickname {
            self.nickname = v.clone();
        }
        if let Some(v) = &patch.first_name {
            self.first_name = v.clone();
        }
        if let Some(v) = &patch.last_name {
            self.last_name = v.clone();
        }
        if let Some(v) = &patch.position {
            self.position = v.clone();
        }
        if let Some(v) = &patch.email {
            self.email = v.clone();
        }
        if patch.props.is_some() {
            self.props = patch.props.clone();
        }
        if patch.notify_props.is_some() {
            self.notify_props = patch.notify_props.clone();
        }
        if let Some(v) = &patch.locale {
            self.locale = v.clone();
        }
        if patch.timezone.is_some() {
            self.timezone = patch.timezone.clone();
        }
        if patch.remote_id.is_some() {
            self.remote_id = patch.remote_id.clone();
        }
    }

    /// Port of `(*User).ToPatch` (user.go:1013). Note Go does **not** copy `RemoteId`.
    pub fn to_patch(&self) -> UserPatch {
        UserPatch {
            username: Some(self.username.clone()),
            password: Some(self.password.clone()),
            nickname: Some(self.nickname.clone()),
            first_name: Some(self.first_name.clone()),
            last_name: Some(self.last_name.clone()),
            position: Some(self.position.clone()),
            email: Some(self.email.clone()),
            props: self.props.clone(),
            notify_props: self.notify_props.clone(),
            locale: Some(self.locale.clone()),
            timezone: self.timezone.clone(),
            remote_id: None,
        }
    }

    /// Port of `(*User).PreUpdate` (user.go:554).
    ///
    /// Go sanitizes the name fields, then does it a second time verbatim (user.go:565-568);
    /// `SanitizeUnicode` is idempotent so the repeat is a no-op and is not reproduced.
    ///
    /// **Incomplete:** the trailing custom-status re-save (user.go:588-594) needs
    /// `custom_status.go`. Callers that rely on custom status must not use this yet.
    pub fn pre_update(&mut self) {
        self.username = sanitize_unicode(&self.username);
        self.first_name = sanitize_unicode(&self.first_name);
        self.last_name = sanitize_unicode(&self.last_name);
        self.nickname = sanitize_unicode(&self.nickname);
        self.bot_description = sanitize_unicode(&self.bot_description);

        self.username = normalize_username(&self.username);
        self.email = normalize_email(&self.email);
        self.update_at = get_millis();

        if self.auth_data.as_deref() == Some("") {
            self.auth_data = None;
        }

        let has_keys = self
            .notify_props
            .as_ref()
            .is_some_and(|p| p.contains_key(MENTION_KEYS_NOTIFY_PROP));

        if self.notify_props.as_ref().is_none_or(StringMap::is_empty) {
            self.set_default_notifications();
        } else if has_keys {
            // Drop blank mention keys and lowercase the rest.
            let cleaned = self
                .notify_props
                .as_ref()
                .and_then(|p| p.get(MENTION_KEYS_NOTIFY_PROP))
                .map_or(String::new(), |raw| {
                    raw.split(',')
                        .filter(|k| !k.is_empty())
                        .map(utils::go_to_lower)
                        .collect::<Vec<_>>()
                        .join(",")
                });
            if let Some(props) = self.notify_props.as_mut() {
                props.insert(MENTION_KEYS_NOTIFY_PROP.into(), cleaned);
            }
        }
    }

    /// The `PreSave` steps that do not depend on un-ported code (user.go:486-531).
    ///
    /// Password hashing, the `timezones.DefaultUserTimezone()` default and the custom-status
    /// re-save are **not** applied — see the module docs. This is deliberately not named
    /// `pre_save`: it is not a drop-in replacement, and calling it on a write path expecting
    /// Go's behaviour would silently store an unhashed password.
    pub fn pre_save_partial(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        if self.username.is_empty() {
            self.username = new_username();
        }
        if self.auth_data.as_deref() == Some("") {
            self.auth_data = None;
        }

        self.username = sanitize_unicode(&self.username);
        self.first_name = sanitize_unicode(&self.first_name);
        self.last_name = sanitize_unicode(&self.last_name);
        self.nickname = sanitize_unicode(&self.nickname);

        self.username = normalize_username(&self.username);
        self.email = normalize_email(&self.email);

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
        self.last_password_update = self.create_at;
        self.mfa_active = false;

        if self.locale.is_empty() {
            self.locale = DEFAULT_LOCALE.to_string();
        }
        self.props.get_or_insert_with(StringMap::new);

        if self.notify_props.as_ref().is_none_or(StringMap::is_empty) {
            self.set_default_notifications();
        }
    }

    // -- sanitization -------------------------------------------------------

    /// Port of `(*User).IsValid` (user.go:383). Closes the `IsValid` half of [D-002].
    ///
    /// Eighteen branches, checked in order — a user broken two ways reports the **first** failure,
    /// which the corpus drives explicitly.
    ///
    /// # The caps mix bytes and runes
    ///
    /// `Email`, `AuthData` and `Roles` are capped in **bytes** (`len`); `Nickname`, `Position`,
    /// `FirstName`, `LastName` and the marshalled timezone in **runes**. The constant names carry
    /// the distinction — `MaxLength` against `MaxRunes` — but `Email` and `Roles` read like they
    /// should count characters and do not. Measured at both boundaries with multi-byte input.
    ///
    /// # A remote user may hold an invalid email
    ///
    /// ```text
    /// len(u.Email) > max || u.Email == "" || (!IsValidEmail(u.Email) && !u.IsRemote())
    /// ```
    ///
    /// Emptiness and length apply to everyone; only the **format** check is skipped when the user
    /// is remote. So a synced user can carry something that is not an email at all, and hoisting
    /// `IsValidEmail` out of that conjunction would reject rows Go accepts. Driven both ways.
    ///
    /// # The timezone cap measures Go's JSON
    ///
    /// It marshals the map and counts the **runes of the result**, so braces, quotes, colons and
    /// commas all count — and Go escapes HTML, so a `<` costs six runes rather than one. Hence
    /// [`crate::utils::go_json_marshal_string_map`] rather than any convenient stringification.
    ///
    /// # `Props` gates the custom-status check
    ///
    /// A **nil** `Props` skips it entirely; an empty-but-present map does not. Three states, and
    /// the middle one is easy to lose in an `Option` that gets `unwrap_or_default`ed.
    pub fn is_valid(&self) -> utils::AppResult {
        if !crate::utils::is_valid_id(&self.id) {
            return Err(Box::new(invalid_user_error("id", "", &self.id)));
        }

        if self.create_at == 0 {
            return Err(Box::new(invalid_user_error(
                "create_at",
                &self.id,
                &self.create_at.to_string(),
            )));
        }

        if self.update_at == 0 {
            return Err(Box::new(invalid_user_error(
                "update_at",
                &self.id,
                &self.update_at.to_string(),
            )));
        }

        // A remote user's username may contain what a local one's may not.
        let username_ok = if self.is_remote() {
            is_valid_username_allow_remote(&self.username)
        } else {
            is_valid_username(&self.username)
        };
        if !username_ok {
            return Err(Box::new(invalid_user_error(
                "username",
                &self.id,
                &self.username,
            )));
        }

        // Bytes, not runes — and the format check is the only part remote users skip.
        if self.email.len() > USER_EMAIL_MAX_LENGTH
            || self.email.is_empty()
            || (!crate::utils::is_valid_email(&self.email) && !self.is_remote())
        {
            return Err(Box::new(invalid_user_error("email", &self.id, &self.email)));
        }

        if self.nickname.chars().count() > USER_NICKNAME_MAX_RUNES {
            return Err(Box::new(invalid_user_error(
                "nickname",
                &self.id,
                &self.nickname,
            )));
        }

        if self.position.chars().count() > USER_POSITION_MAX_RUNES {
            return Err(Box::new(invalid_user_error(
                "position",
                &self.id,
                &self.position,
            )));
        }

        if self.first_name.chars().count() > USER_FIRST_NAME_MAX_RUNES {
            return Err(Box::new(invalid_user_error(
                "first_name",
                &self.id,
                &self.first_name,
            )));
        }

        if self.last_name.chars().count() > USER_LAST_NAME_MAX_RUNES {
            return Err(Box::new(invalid_user_error(
                "last_name",
                &self.id,
                &self.last_name,
            )));
        }

        if let Some(auth_data) = &self.auth_data
            && auth_data.len() > USER_AUTH_DATA_MAX_LENGTH
        {
            // Go passes the **pointer** here, not the string, so its detail contains a memory
            // address that changes between calls. Unreproducible by construction, and emitting
            // the value instead would put an SSO identifier in the logs that Go keeps out of
            // them. A marker, and [D-107].
            return Err(Box::new(invalid_user_error(
                "auth_data",
                &self.id,
                "<pointer>",
            )));
        }

        if let Some(auth_data) = &self.auth_data
            && !auth_data.is_empty()
            && self.auth_service.is_empty()
        {
            return Err(Box::new(invalid_user_error(
                "auth_data_type",
                &self.id,
                &format!("{auth_data} {}", self.auth_service),
            )));
        }

        if !self.password.is_empty()
            && let Some(auth_data) = &self.auth_data
            && !auth_data.is_empty()
        {
            return Err(Box::new(invalid_user_error(
                "auth_data_pwd",
                &self.id,
                auth_data,
            )));
        }

        if !is_valid_locale(&self.locale) {
            return Err(Box::new(invalid_user_error(
                "locale",
                &self.id,
                &self.locale,
            )));
        }

        // `len(u.Timezone) > 0` is the map's entry count, so a present-but-empty map skips this.
        if let Some(timezone) = &self.timezone
            && !timezone.is_empty()
        {
            let marshalled = crate::utils::go_json_marshal_string_map(Some(timezone));
            if marshalled.chars().count() > USER_TIMEZONE_MAX_RUNES {
                return Err(Box::new(invalid_user_error(
                    "timezone_limit",
                    &self.id,
                    &crate::utils::go_format_string_map(timezone),
                )));
            }
        }

        // Bytes. And this branch builds its error directly rather than through
        // `InvalidUserError`, so the id shape and the params differ from every branch above.
        if self.roles.len() > USER_ROLES_MAX_LENGTH {
            let mut params = std::collections::HashMap::new();
            params.insert(
                "Limit".to_owned(),
                serde_json::Value::from(USER_ROLES_MAX_LENGTH),
            );
            return Err(Box::new(utils::AppError::new(
                "User.IsValid",
                "model.user.is_valid.roles_limit.app_error",
                Some(params),
                format!("user_id={} roles_limit={}", self.id, self.roles),
                400,
            )));
        }

        // A nil `Props` skips the check; an empty one does not.
        if let Some(props) = &self.props
            && !self.validate_custom_status()
        {
            let mut params = std::collections::HashMap::new();
            params.insert(
                "Props".to_owned(),
                serde_json::to_value(props).unwrap_or(serde_json::Value::Null),
            );
            return Err(Box::new(utils::AppError::new(
                "User.IsValid",
                "model.user.is_valid.invalidProperty.app_error",
                Some(params),
                format!("user_id={}", self.id),
                400,
            )));
        }

        Ok(())
    }

    /// Port of `(*User).Sanitize` (user.go:696).
    ///
    /// `options` is Go's `map[string]bool`; an **empty** map means "strip nothing extra",
    /// while a populated map strips every field whose flag is absent or false.
    pub fn sanitize(&mut self, options: &HashMap<String, bool>) {
        self.password.clear();
        self.mfa_secret.clear();
        self.mfa_used_timestamps = None;
        self.last_login = 0;

        if options.is_empty() {
            return;
        }
        let allowed = |key: &str| options.get(key).copied().unwrap_or(false);

        if !allowed("email") {
            self.email.clear();
            if let Some(props) = self.props.as_mut() {
                props.remove(USER_PROPS_KEY_REMOTE_EMAIL);
            }
        }
        if !allowed("fullname") {
            self.first_name.clear();
            self.last_name.clear();
        }
        if !allowed("passwordupdate") {
            self.last_password_update = 0;
        }
        if !allowed("authservice") {
            self.auth_service.clear();
        }
        if !allowed("authdata") {
            // Go sets a pointer to "", which still serialises as "auth_data": "".
            self.auth_data = Some(String::new());
        }
    }

    /// Port of `(*User).SanitizeInput` (user.go:724).
    pub fn sanitize_input(&mut self, is_admin: bool) {
        if !is_admin {
            self.auth_data = Some(String::new());
            self.auth_service.clear();
            self.email_verified = false;
        }
        self.remote_id = Some(String::new());
        self.create_at = 0;
        self.update_at = 0;
        self.delete_at = 0;
        self.last_password_update = 0;
        self.last_picture_update = 0;
        self.failed_attempts = 0;
        self.mfa_active = false;
        self.mfa_secret.clear();
        self.mfa_used_timestamps = Some(StringArray::new());
        self.email = self.email.trim().to_string();
        self.last_activity_at = 0;
    }

    /// Port of `(*User).ClearNonProfileFields` (user.go:744).
    pub fn clear_non_profile_fields(&mut self, as_admin: bool) {
        self.password.clear();
        self.mfa_secret.clear();
        self.mfa_used_timestamps = None;
        self.email_verified = false;
        self.allow_marketing = false;
        self.last_password_update = 0;

        if !as_admin {
            self.auth_data = Some(String::new());
            self.notify_props = Some(StringMap::new());
            self.failed_attempts = 0;
        }
    }

    /// Port of `(*User).SanitizeProfile` (user.go:759).
    pub fn sanitize_profile(&mut self, options: &HashMap<String, bool>, as_admin: bool) {
        self.clear_non_profile_fields(as_admin);
        self.sanitize(options);
    }
}

impl UserPatch {
    /// Port of `(*UserPatch).SetField` (user.go:1023). Unknown names are ignored, as in Go.
    pub fn set_field(&mut self, field_name: &str, field_value: impl Into<String>) {
        let value = Some(field_value.into());
        match field_name {
            "FirstName" => self.first_name = value,
            "LastName" => self.last_name = value,
            "Nickname" => self.nickname = value,
            "Email" => self.email = value,
            "Position" => self.position = value,
            "Username" => self.username = value,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Port of `model.NormalizeUsername` (user.go:475).
///
/// Go's `strings.ToLower`, which is not `str::to_lowercase` — see [`crate::utils::go_to_lower`].
pub fn normalize_username(username: &str) -> String {
    utils::go_to_lower(username)
}

/// Port of `model.NormalizeEmail` (user.go:479).
///
/// Go's `strings.ToLower`, which is not `str::to_lowercase` — see [`crate::utils::go_to_lower`].
pub fn normalize_email(email: &str) -> String {
    utils::go_to_lower(email)
}

/// Port of `model.IsInRole` (user.go:914).
///
/// Splits on a **single space**, unlike `GetRoles`, which uses `strings.Fields`. A roles
/// string with a tab or double space therefore behaves differently between the two. Faithful
/// to Go.
pub fn is_in_role(user_roles: &str, in_role: &str) -> bool {
    user_roles.split(' ').any(|r| r == in_role)
}

/// Port of `restrictedUsernames` (user.go:1043).
const RESTRICTED_USERNAMES: [&str; 4] = ["all", "channel", "matterbot", "system"];

static VALID_USERNAME_CHARS: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9\.\-_]+$").ok());
static VALID_USERNAME_CHARS_FOR_REMOTE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9\.\-_:]*$").ok());

/// Port of `model.IsValidUsername` (user.go:1050). Length is measured in **bytes**.
pub fn is_valid_username(s: &str) -> bool {
    if s.len() < USER_NAME_MIN_LENGTH || s.len() > USER_NAME_MAX_LENGTH {
        return false;
    }
    if !VALID_USERNAME_CHARS
        .as_ref()
        .is_some_and(|re| re.is_match(s))
    {
        return false;
    }
    !RESTRICTED_USERNAMES.contains(&s)
}

/// Port of `model.IsValidUsernameAllowRemote` (user.go:1063).
///
/// The remote pattern ends in `*` rather than `+`, so it accepts the empty string — but the
/// length check above rejects it first. Kept as-is to match Go exactly.
pub fn is_valid_username_allow_remote(s: &str) -> bool {
    if s.len() < USER_NAME_MIN_LENGTH || s.len() > USER_NAME_MAX_LENGTH {
        return false;
    }
    if !VALID_USERNAME_CHARS_FOR_REMOTE
        .as_ref()
        .is_some_and(|re| re.is_match(s))
    {
        return false;
    }
    !RESTRICTED_USERNAMES.contains(&s)
}

/// Port of `model.IsValidUserAuthService` (user.go:942).
///
/// **Unverified against Go**: the Go body was not read this session. The accepted set is
/// inferred from the auth-service constants and is asserted only by `UserAuth::is_valid`
/// tests. Confirm when `ldap.go`/`saml.go` are translated.
pub fn is_valid_user_auth_service(service: &str) -> bool {
    matches!(
        service,
        USER_AUTH_SERVICE_EMAIL
            | USER_AUTH_SERVICE_LDAP
            | USER_AUTH_SERVICE_SAML
            | SERVICE_GITLAB
            | SERVICE_GOOGLE
            | SERVICE_OFFICE365
            | SERVICE_OPENID
    )
}

/// Port of `model.InvalidUserError` (user.go:465).
///
/// Note the space Go leaves in the details when `user_id` is empty: the format string always
/// starts with `" %s=%v"`, so a missing user id yields a leading space.
pub fn invalid_user_error(field_name: &str, user_id: &str, field_value: &str) -> utils::AppError {
    let details = if user_id.is_empty() {
        format!(" {field_name}={field_value}")
    } else {
        format!("user_id={user_id} {field_name}={field_value}")
    };
    utils::AppError::new(
        "User.IsValid",
        format!("model.user.is_valid.{field_name}.app_error"),
        None,
        details,
        400,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_user() -> User {
        serde_json::from_str(include_str!("../../../fixtures/user.json")).unwrap()
    }

    #[test]
    fn user_matches_go_serialization() {
        let go = include_str!("../../../fixtures/user.json");
        let parsed: User = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
    }

    #[test]
    fn fixture_covers_every_field() {
        // Guards the oracle itself: if the generator ever emits a partial user, the parity
        // test above would still pass while proving less.
        let go: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/user.json")).unwrap();
        assert_eq!(
            go.as_object().unwrap().len(),
            35,
            "user.json field count changed"
        );
    }

    #[test]
    fn empty_user_omits_every_omitempty_field() {
        let value = serde_json::to_value(User::default()).unwrap();
        let object = value.as_object().unwrap();

        // No omitempty in Go -> always present, even at zero.
        for key in [
            "id",
            "delete_at",
            "username",
            "auth_service",
            "email",
            "nickname",
            "first_name",
            "last_name",
            "position",
            "roles",
            "locale",
            "timezone",
            "disable_welcome_email",
        ] {
            assert!(object.contains_key(key), "{key} should always serialise");
        }
        // omitempty in Go -> absent at zero.
        for key in [
            "create_at",
            "update_at",
            "password",
            "auth_data",
            "email_verified",
            "allow_marketing",
            "props",
            "notify_props",
            "mfa_secret",
            "remote_id",
            "is_bot",
            "last_login",
            "mfa_used_timestamps",
        ] {
            assert!(!object.contains_key(key), "{key} should be omitted at zero");
        }
        assert_eq!(object.len(), 13);
    }

    #[test]
    fn nil_timezone_serialises_as_null_not_empty_object() {
        // Go: Timezone has no omitempty, so a nil map marshals to null.
        let value = serde_json::to_value(User::default()).unwrap();
        assert!(value["timezone"].is_null());

        let user = User {
            timezone: Some(StringMap::new()),
            ..Default::default()
        };
        let value = serde_json::to_value(&user).unwrap();
        assert!(value["timezone"].is_object());
    }

    #[test]
    fn empty_props_map_is_omitted_like_gos_omitempty() {
        let user = User {
            props: Some(StringMap::new()),
            ..Default::default()
        };
        let value = serde_json::to_value(&user).unwrap();
        // Go's omitempty drops len == 0 maps, not just nil ones.
        assert!(!value.as_object().unwrap().contains_key("props"));
    }

    #[test]
    fn empty_auth_data_pointer_still_serialises() {
        let user = User {
            auth_data: Some(String::new()),
            ..Default::default()
        };
        let value = serde_json::to_value(&user).unwrap();
        assert_eq!(value["auth_data"], "");
    }

    #[test]
    fn roles_helpers_split_differently() {
        // A double space is harmless: Split(" ") yields an empty middle element, and both
        // real roles still match.
        let mut user = User {
            roles: "system_user  system_admin".into(),
            ..Default::default()
        };
        assert_eq!(user.get_roles(), vec!["system_user", "system_admin"]);
        assert!(user.is_in_role("system_user"));
        assert!(user.is_in_role("system_admin"));
        assert_eq!(user.get_raw_roles(), "system_user  system_admin");

        // A tab is where the two genuinely diverge: strings.Fields splits on it, but
        // IsInRole's Split(" ") does not, so the roles never match.
        user.roles = "system_user\tsystem_admin".into();
        assert_eq!(user.get_roles(), vec!["system_user", "system_admin"]);
        assert!(!user.is_in_role("system_user"));
        assert!(!user.is_in_role("system_admin"));
    }

    #[test]
    fn role_predicates() {
        let mut user = User {
            roles: "system_user system_guest".into(),
            ..Default::default()
        };
        assert!(user.is_guest());
        assert!(!user.is_system_admin());

        user.roles = "system_admin".into();
        assert!(user.is_system_admin());
        assert!(!user.is_guest());
    }

    #[test]
    fn magic_link_requires_guest_role() {
        let mut user = User {
            auth_service: USER_AUTH_SERVICE_MAGIC_LINK.into(),
            ..Default::default()
        };
        assert!(!user.is_magic_link_enabled(), "not a guest");

        user.roles = "system_guest".into();
        assert!(user.is_magic_link_enabled());
    }

    #[test]
    fn auth_service_predicates() {
        let mut user = User::default();
        assert!(!user.is_sso_user(), "empty auth service is not SSO");

        user.auth_service = USER_AUTH_SERVICE_EMAIL.into();
        assert!(!user.is_sso_user(), "email auth is not SSO");

        for service in [
            SERVICE_GITLAB,
            SERVICE_GOOGLE,
            SERVICE_OFFICE365,
            SERVICE_OPENID,
        ] {
            user.auth_service = service.into();
            assert!(user.is_oauth_user(), "{service} should be OAuth");
            assert!(user.is_sso_user());
        }

        user.auth_service = USER_AUTH_SERVICE_LDAP.into();
        assert!(user.is_ldap_user() && !user.is_oauth_user());

        user.auth_service = USER_AUTH_SERVICE_SAML.into();
        assert!(user.is_saml_user() && !user.is_oauth_user());
    }

    #[test]
    fn remote_and_auth_data_dereference_safely() {
        let mut user = User::default();
        assert!(!user.is_remote());
        assert_eq!(user.get_remote_id(), "");
        assert_eq!(user.get_auth_data(), "");

        user.remote_id = Some(String::new());
        assert!(!user.is_remote(), "empty string is not remote");

        user.remote_id = Some("remote1".into());
        assert!(user.is_remote());
        assert_eq!(user.get_remote_id(), "remote1");
    }

    #[test]
    fn original_remote_id_covers_every_branch() {
        let mut user = User::default();
        // nil props, local user
        assert_eq!(user.get_original_remote_id(), "");

        // nil props, remote user
        user.remote_id = Some("r1".into());
        assert_eq!(
            user.get_original_remote_id(),
            USER_ORIGINAL_REMOTE_ID_UNKNOWN
        );

        // props present with the key set
        user.set_prop(USER_PROPS_KEY_ORIGINAL_REMOTE_ID, "origin1");
        assert_eq!(user.get_original_remote_id(), "origin1");

        // props present but the key is blank -> falls through to remote check
        user.set_prop(USER_PROPS_KEY_ORIGINAL_REMOTE_ID, "");
        assert_eq!(
            user.get_original_remote_id(),
            USER_ORIGINAL_REMOTE_ID_UNKNOWN
        );

        // props present, blank key, local user
        user.remote_id = None;
        assert_eq!(user.get_original_remote_id(), "");
    }

    #[test]
    fn props_accessors_create_the_map_lazily() {
        let mut user = User::default();
        assert_eq!(user.get_prop("k"), None);
        assert!(user.props.is_none());

        user.set_prop("k", "v");
        assert_eq!(user.get_prop("k"), Some("v"));
        assert!(user.props.is_some());
    }

    #[test]
    fn make_non_nil_initialises_both_maps() {
        let mut user = User::default();
        user.make_non_nil();
        assert_eq!(user.props.as_ref().map(StringMap::len), Some(0));
        assert_eq!(user.notify_props.as_ref().map(StringMap::len), Some(0));
    }

    #[test]
    fn set_default_notifications_matches_go_exactly() {
        let mut user = User::default();
        user.set_default_notifications();
        let props = user.notify_props.unwrap();

        assert_eq!(props.len(), 13);
        assert_eq!(props["email"], "true");
        assert_eq!(props["push"], "mention");
        assert_eq!(props["desktop"], "mention");
        assert_eq!(props["desktop_sound"], "true");
        assert_eq!(props["mention_keys"], "");
        assert_eq!(props["channel"], "true");
        assert_eq!(props["push_status"], "online");
        assert_eq!(props["comments"], "never");
        assert_eq!(props["first_name"], "false");
        assert_eq!(props["desktop_threads"], "all");
        assert_eq!(props["email_threads"], "all");
        assert_eq!(props["push_threads"], "all");
        assert_eq!(props["channel_mention_auto_follow_threads"], "true");
    }

    #[test]
    fn get_mention_keys_trims_and_drops_blanks() {
        let mut user = User::default();
        assert!(user.get_mention_keys().is_empty(), "nil notify props");

        user.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, "");
        assert!(user.get_mention_keys().is_empty());

        user.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, "one, two ,,  three  ,");
        assert_eq!(user.get_mention_keys(), vec!["one", "two", "three"]);
    }

    #[test]
    fn update_mention_keys_from_username_keeps_gos_leading_comma() {
        let mut user = User::default();
        user.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, "oldname,@oldname,keepme");
        user.update_mention_keys_from_username("oldname");

        // Go builds "" + "," + join(kept) — the leading comma is real.
        assert_eq!(
            user.notify_props.as_ref().unwrap()[MENTION_KEYS_NOTIFY_PROP],
            ",keepme"
        );
        assert_eq!(user.get_mention_keys(), vec!["keepme"]);
    }

    #[test]
    fn update_mention_keys_clears_when_nothing_survives() {
        let mut user = User::default();
        user.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, "oldname,@oldname");
        user.update_mention_keys_from_username("oldname");
        assert_eq!(
            user.notify_props.as_ref().unwrap()[MENTION_KEYS_NOTIFY_PROP],
            ""
        );
    }

    #[test]
    fn full_name_covers_every_branch() {
        let mut user = User::default();
        assert_eq!(user.get_full_name(), "");

        user.first_name = "Ada".into();
        assert_eq!(user.get_full_name(), "Ada");

        user.last_name = "Lovelace".into();
        assert_eq!(user.get_full_name(), "Ada Lovelace");

        user.first_name.clear();
        assert_eq!(user.get_full_name(), "Lovelace");
    }

    #[test]
    fn display_name_honours_the_name_format() {
        let mut user = User {
            username: "ada".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            nickname: "countess".into(),
            ..Default::default()
        };

        assert_eq!(user.get_display_name(SHOW_USERNAME), "ada");
        assert_eq!(user.get_display_name(SHOW_FULL_NAME), "Ada Lovelace");
        assert_eq!(user.get_display_name(SHOW_NICKNAME_FULL_NAME), "countess");

        // Nickname format falls back to full name, then to the base name.
        user.nickname.clear();
        assert_eq!(
            user.get_display_name(SHOW_NICKNAME_FULL_NAME),
            "Ada Lovelace"
        );
        user.first_name.clear();
        user.last_name.clear();
        assert_eq!(user.get_display_name(SHOW_NICKNAME_FULL_NAME), "ada");
        assert_eq!(user.get_display_name(SHOW_FULL_NAME), "ada");
    }

    #[test]
    fn display_name_with_prefix_applies_to_the_base_only() {
        let user = User {
            username: "ada".into(),
            first_name: "Ada".into(),
            ..Default::default()
        };

        assert_eq!(
            user.get_display_name_with_prefix(SHOW_USERNAME, "@"),
            "@ada"
        );
        // When the full name wins, the prefix is dropped — the prefix only decorates the base.
        assert_eq!(
            user.get_display_name_with_prefix(SHOW_FULL_NAME, "@"),
            "Ada"
        );
    }

    #[test]
    fn email_domain_extraction() {
        let mut user = User::default();
        assert_eq!(user.email_domain(), "");

        user.email = "ada@example.com".into();
        assert_eq!(user.email_domain(), "example.com");

        user.email = "no-at-sign".into();
        assert_eq!(user.email_domain(), "");
    }

    #[test]
    fn preferred_timezone_handles_a_nil_map() {
        let mut user = User::default();
        assert_eq!(user.get_preferred_timezone(), "");

        let mut tz = StringMap::new();
        tz.insert("useAutomaticTimezone".into(), "true".into());
        tz.insert("automaticTimezone".into(), "Europe/Berlin".into());
        user.timezone = Some(tz);
        assert_eq!(user.get_preferred_timezone(), "Europe/Berlin");
    }

    // -- patching -----------------------------------------------------------

    #[test]
    fn patch_applies_only_present_fields() {
        let mut user = fixture_user();
        let original_email = user.email.clone();

        let patch = UserPatch {
            username: Some("newname".into()),
            ..Default::default()
        };
        user.patch(&patch);

        assert_eq!(user.username, "newname");
        assert_eq!(user.email, original_email, "absent fields are untouched");
    }

    #[test]
    fn patch_can_set_empty_values() {
        let mut user = fixture_user();
        let patch = UserPatch {
            nickname: Some(String::new()),
            ..Default::default()
        };
        user.patch(&patch);
        assert_eq!(user.nickname, "");
    }

    #[test]
    fn to_patch_drops_the_remote_id() {
        // Go's ToPatch does not carry RemoteId across; a round trip must not resurrect it.
        let mut user = fixture_user();
        user.remote_id = Some("r1".into());
        let patch = user.to_patch();
        assert!(patch.remote_id.is_none());
        assert_eq!(patch.username, Some(user.username.clone()));
    }

    #[test]
    fn set_field_maps_go_field_names() {
        let mut patch = UserPatch::default();
        patch.set_field("FirstName", "Ada");
        patch.set_field("Email", "ada@example.com");
        patch.set_field("Unknown", "ignored");

        assert_eq!(patch.first_name, Some("Ada".into()));
        assert_eq!(patch.email, Some("ada@example.com".into()));
        assert_eq!(patch.last_name, None);
    }

    // -- sanitization -------------------------------------------------------

    fn options(pairs: &[(&str, bool)]) -> HashMap<String, bool> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn sanitize_with_empty_options_strips_only_secrets() {
        let mut user = fixture_user();
        let email = user.email.clone();
        user.sanitize(&HashMap::new());

        assert_eq!(user.password, "");
        assert_eq!(user.mfa_secret, "");
        assert!(user.mfa_used_timestamps.is_none());
        assert_eq!(user.last_login, 0);
        // An empty options map means nothing else is stripped.
        assert_eq!(user.email, email);
    }

    #[test]
    fn sanitize_strips_fields_whose_flag_is_absent_or_false() {
        let mut user = fixture_user();
        user.set_prop(USER_PROPS_KEY_REMOTE_EMAIL, "remote@example.com");
        user.sanitize(&options(&[("email", false), ("fullname", true)]));

        assert_eq!(user.email, "", "email flag false -> stripped");
        assert_eq!(
            user.get_prop(USER_PROPS_KEY_REMOTE_EMAIL),
            None,
            "remote email prop removed with the email"
        );
        assert_ne!(user.first_name, "", "fullname flag true -> kept");
        assert_eq!(user.last_password_update, 0, "flag absent -> stripped");
        assert_eq!(user.auth_service, "");
        assert_eq!(
            user.auth_data,
            Some(String::new()),
            "Go sets a pointer to empty, not nil"
        );
    }

    #[test]
    fn sanitize_keeps_everything_flagged_true() {
        let mut user = fixture_user();
        let before = user.clone();
        user.sanitize(&options(&[
            ("email", true),
            ("fullname", true),
            ("passwordupdate", true),
            ("authservice", true),
            ("authdata", true),
        ]));

        assert_eq!(user.email, before.email);
        assert_eq!(user.first_name, before.first_name);
        assert_eq!(user.last_password_update, before.last_password_update);
        assert_eq!(user.auth_service, before.auth_service);
        assert_eq!(user.auth_data, before.auth_data);
    }

    #[test]
    fn sanitize_input_clears_server_controlled_fields() {
        let mut user = fixture_user();
        user.email = "  ada@example.com  ".into();
        user.sanitize_input(false);

        assert_eq!(user.auth_data, Some(String::new()));
        assert_eq!(user.auth_service, "");
        assert!(!user.email_verified);
        assert_eq!(user.remote_id, Some(String::new()));
        assert_eq!(user.create_at, 0);
        assert_eq!(user.update_at, 0);
        assert_eq!(user.delete_at, 0);
        assert_eq!(user.failed_attempts, 0);
        assert!(!user.mfa_active);
        assert_eq!(user.mfa_used_timestamps, Some(StringArray::new()));
        assert_eq!(user.email, "ada@example.com", "trimmed");
        assert_eq!(user.last_activity_at, 0);
    }

    #[test]
    fn sanitize_input_as_admin_keeps_auth_fields() {
        let mut user = fixture_user();
        let auth_service = user.auth_service.clone();
        let auth_data = user.auth_data.clone();
        user.email_verified = true;

        user.sanitize_input(true);

        assert_eq!(user.auth_service, auth_service);
        assert_eq!(user.auth_data, auth_data);
        assert!(user.email_verified, "admins keep email_verified");
        // Non-admin-gated fields are still cleared.
        assert_eq!(user.remote_id, Some(String::new()));
    }

    #[test]
    fn clear_non_profile_fields_differs_by_admin() {
        let mut user = fixture_user();
        user.clear_non_profile_fields(true);
        assert_eq!(user.password, "");
        assert!(!user.email_verified);
        assert!(!user.allow_marketing);
        assert_eq!(user.last_password_update, 0);
        assert!(user.notify_props.is_some(), "admin keeps notify props");

        let mut user = fixture_user();
        user.clear_non_profile_fields(false);
        assert_eq!(user.auth_data, Some(String::new()));
        assert_eq!(user.notify_props, Some(StringMap::new()));
        assert_eq!(user.failed_attempts, 0);
    }

    #[test]
    fn sanitize_profile_composes_both_steps() {
        let mut user = fixture_user();
        user.sanitize_profile(&options(&[("email", true)]), false);

        assert_eq!(user.password, "");
        assert_eq!(user.notify_props, Some(StringMap::new()));
        assert_ne!(user.email, "", "email flag true");
        assert_eq!(user.last_password_update, 0);
    }

    // -- lifecycle ----------------------------------------------------------

    #[test]
    fn pre_save_partial_fills_identity_and_timestamps() {
        let mut user = User {
            email: "ADA@Example.COM".into(),
            ..Default::default()
        };
        user.pre_save_partial();

        assert!(utils::is_valid_id(&user.id));
        assert_eq!(user.username.len(), 27, "generated username is 'a' + 26");
        assert_eq!(user.email, "ada@example.com", "normalised");
        assert!(user.create_at > 0);
        assert_eq!(user.update_at, user.create_at);
        assert_eq!(user.last_password_update, user.create_at);
        assert_eq!(user.locale, DEFAULT_LOCALE);
        assert!(user.props.is_some());
        assert_eq!(user.notify_props.as_ref().map(StringMap::len), Some(13));
    }

    #[test]
    fn pre_save_partial_preserves_an_existing_id_and_create_at() {
        let mut user = User {
            id: "existingid1234567890abcde".into(),
            create_at: 12345,
            username: "Ada".into(),
            ..Default::default()
        };
        user.pre_save_partial();

        assert_eq!(user.id, "existingid1234567890abcde");
        assert_eq!(user.create_at, 12345);
        assert_eq!(user.update_at, 12345);
        assert_eq!(user.username, "ada");
    }

    #[test]
    fn pre_save_partial_nils_an_empty_auth_data_pointer() {
        let mut user = User {
            auth_data: Some(String::new()),
            ..Default::default()
        };
        user.pre_save_partial();
        assert!(user.auth_data.is_none());
    }

    #[test]
    fn pre_update_normalises_and_cleans_mention_keys() {
        let mut user = User {
            username: "ADA".into(),
            email: "ADA@Example.COM".into(),
            ..Default::default()
        };
        user.add_notify_prop(MENTION_KEYS_NOTIFY_PROP, "One,,TWO,");
        user.pre_update();

        assert_eq!(user.username, "ada");
        assert_eq!(user.email, "ada@example.com");
        assert!(user.update_at > 0);
        assert_eq!(
            user.notify_props.as_ref().unwrap()[MENTION_KEYS_NOTIFY_PROP],
            "one,two"
        );
    }

    #[test]
    fn pre_update_sets_defaults_when_notify_props_are_empty() {
        let mut user = User::default();
        user.pre_update();
        assert_eq!(user.notify_props.as_ref().map(StringMap::len), Some(13));
    }

    #[test]
    fn pre_update_strips_unicode_from_names() {
        let mut user = User {
            username: "ad\u{202E}a".into(), // BIDI override
            bot_description: "bot\u{FEFF}desc".into(),
            ..Default::default()
        };
        user.pre_update();
        assert_eq!(user.username, "ada");
        assert_eq!(user.bot_description, "botdesc");
    }

    // -- UserAuth -----------------------------------------------------------

    #[test]
    fn user_auth_is_valid_covers_every_branch() {
        // Unknown service.
        let auth = UserAuth {
            auth_service: "nope".into(),
            auth_data: Some("x".into()),
        };
        assert!(!auth.is_valid());

        // Email auth must have NO auth data.
        let auth = UserAuth {
            auth_service: USER_AUTH_SERVICE_EMAIL.into(),
            auth_data: None,
        };
        assert!(auth.is_valid());

        let auth = UserAuth {
            auth_service: USER_AUTH_SERVICE_EMAIL.into(),
            auth_data: Some(String::new()),
        };
        assert!(!auth.is_valid(), "email auth with a non-nil pointer");

        // Other services require non-empty auth data.
        let auth = UserAuth {
            auth_service: USER_AUTH_SERVICE_LDAP.into(),
            auth_data: Some("cn=ada".into()),
        };
        assert!(auth.is_valid());

        let auth = UserAuth {
            auth_service: USER_AUTH_SERVICE_LDAP.into(),
            auth_data: Some(String::new()),
        };
        assert!(!auth.is_valid());

        let auth = UserAuth {
            auth_service: USER_AUTH_SERVICE_LDAP.into(),
            auth_data: None,
        };
        assert!(!auth.is_valid());
    }

    // -- free functions -----------------------------------------------------

    #[test]
    fn normalize_lowercases() {
        assert_eq!(normalize_username("AdA"), "ada");
        assert_eq!(normalize_email("Ada@Example.COM"), "ada@example.com");
    }

    #[test]
    fn invalid_user_error_formats_details_like_go() {
        let err = invalid_user_error("email", "abc", "bad@");
        assert_eq!(err.id, "model.user.is_valid.email.app_error");
        assert_eq!(err.detailed_error, "user_id=abc email=bad@");
        assert_eq!(err.status_code, 400);

        // Go's format string always leads with a space, so a blank user id shows through.
        let err = invalid_user_error("id", "", "xyz");
        assert_eq!(err.detailed_error, " id=xyz");
    }
}

/// Differential tests against what the real Go `user.go` functions returned.
///
/// See the module of the same name in `utils.rs`. This one earned its place immediately too:
/// a hand-written test here asserted that a double space in `Roles` made `IsInRole` miss the
/// second role. It does not — `strings.Split("a  b", " ")` yields an empty middle element and
/// both roles still match. Go said so; the reasoning did not.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_utils.json")).unwrap()
    }

    #[test]
    fn is_valid_username_matches_go() {
        let oracle = oracle();
        for (input, want) in oracle["is_valid_username"].as_object().unwrap() {
            assert_eq!(
                is_valid_username(input),
                want.as_bool().unwrap(),
                "IsValidUsername({input:?})"
            );
        }
    }

    #[test]
    fn is_valid_username_allow_remote_matches_go() {
        let oracle = oracle();
        for (input, want) in oracle["is_valid_username_allow_remote"]
            .as_object()
            .unwrap()
        {
            assert_eq!(
                is_valid_username_allow_remote(input),
                want.as_bool().unwrap(),
                "IsValidUsernameAllowRemote({input:?})"
            );
        }
    }

    #[test]
    fn is_in_role_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_in_role"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (key, want) in cases {
            let (roles, wanted_role) = key.rsplit_once('|').unwrap();
            assert_eq!(
                is_in_role(roles, wanted_role),
                want.as_bool().unwrap(),
                "IsInRole({roles:?}, {wanted_role:?})"
            );
        }
    }

    #[test]
    fn get_roles_matches_go() {
        let oracle = oracle();
        for (roles, want) in oracle["get_roles"].as_object().unwrap() {
            let expected: Vec<&str> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let user = User {
                roles: roles.clone(),
                ..Default::default()
            };
            assert_eq!(user.get_roles(), expected, "GetRoles({roles:?})");
        }
    }

    #[test]
    fn display_names_match_go() {
        // Mirrors the `people` table in behaviour.go.
        let people: [(&str, [&str; 4]); 6] = [
            ("full", ["ada", "Ada", "Lovelace", "countess"]),
            ("no_nickname", ["ada", "Ada", "Lovelace", ""]),
            ("first_only", ["ada", "Ada", "", ""]),
            ("last_only", ["ada", "", "Lovelace", ""]),
            ("username_only", ["ada", "", "", ""]),
            ("nickname_only", ["ada", "", "", "countess"]),
        ];

        let oracle = oracle();
        let cases = oracle["user_display_names"].as_object().unwrap();

        for (name, [username, first, last, nickname]) in people {
            let user = User {
                username: username.to_string(),
                first_name: first.to_string(),
                last_name: last.to_string(),
                nickname: nickname.to_string(),
                ..Default::default()
            };

            assert_eq!(
                user.get_full_name(),
                cases[&format!("{name}|fullname")].as_str().unwrap(),
                "GetFullName for {name}"
            );

            for format in [SHOW_USERNAME, SHOW_FULL_NAME, SHOW_NICKNAME_FULL_NAME] {
                assert_eq!(
                    user.get_display_name(format),
                    cases[&format!("{name}|{format}")].as_str().unwrap(),
                    "GetDisplayName({format}) for {name}"
                );
                assert_eq!(
                    user.get_display_name_with_prefix(format, "@"),
                    cases[&format!("{name}|{format}|@")].as_str().unwrap(),
                    "GetDisplayNameWithPrefix({format}, @) for {name}"
                );
            }
        }
    }
}

/// Parity tests for the custom-status accessors, driven by
/// `fixtures/behaviour_custom_status.json`. They live in a module of their own because the
/// corpus belongs to `custom_status.go`'s oracle even though the methods are `user.go`'s.
#[cfg(test)]
mod custom_status_go_parity {
    use super::*;
    use crate::custom_status::USER_PROPS_KEY_CUSTOM_STATUS;
    use crate::utils::go_time;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_custom_status.json"
        ))
        .unwrap()
    }

    fn user_with_prop(case: &Value) -> User {
        let mut user = User::default();
        if case["prop_present"].as_bool().unwrap() {
            user.set_prop(USER_PROPS_KEY_CUSTOM_STATUS, case["prop"].as_str().unwrap());
        }
        user
    }

    /// `ValidateCustomStatus` gates `User::is_valid`, so this is the answer that reaches the
    /// wire. It must match Go on every case, including the ones where `get_custom_status`
    /// deliberately does not (D-026).
    #[test]
    fn validate_custom_status_matches_go() {
        let oracle = oracle();
        let cases = oracle["user_custom_status"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert_eq!(
                user_with_prop(case).validate_custom_status(),
                case["validate"].as_bool().unwrap(),
                "case {name}"
            );
        }
    }

    /// `GetCustomStatus` must agree with Go on **whether** a status comes back for every case,
    /// and on its contents for every case Go decodes cleanly. The exceptions are the
    /// partial-decode cases, which are asserted as the known divergence rather than skipped.
    #[test]
    fn get_custom_status_matches_go() {
        // Go's decoder keeps the fields it managed to decode before a type error or a failing
        // Unmarshaler; serde_json returns Err and we substitute a zero status. See D-026.
        const PARTIAL_DECODE: &[&str] = &[
            "type_error_first",
            "type_error_last",
            "bad_expires_at_middle",
            "bad_expires_at_last",
        ];

        let oracle = oracle();
        let cases = oracle["user_custom_status"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let got = user_with_prop(case).get_custom_status();
            let want = &case["get"];

            if want.is_null() {
                assert!(got.is_none(), "case {name}: got a status, Go got nil");
                continue;
            }
            let got = got.unwrap_or_else(|| panic!("case {name}: got nil, Go got a status"));

            if PARTIAL_DECODE.contains(&name) {
                // Non-nil-ness matches; the fields do not. Pin what we actually produce so the
                // divergence is visible here rather than discovered downstream.
                assert_eq!(got, CustomStatus::default(), "case {name}");
                assert_ne!(*want, serde_json::to_value(&got).unwrap(), "case {name}");
                continue;
            }
            assert_eq!(serde_json::to_value(&got).unwrap(), *want, "case {name}");
        }
    }

    /// The stored string is compared byte-for-byte: it is what lands in the shared
    /// `Users.Props` column, HTML escaping included.
    #[test]
    fn set_custom_status_stores_gos_bytes() {
        let oracle = oracle();
        let cases = oracle["set_custom_status"].as_object().unwrap();
        let recent = go_time::parse("2026-08-14T12:00:00Z").unwrap();

        let inputs: Vec<(&str, CustomStatus)> = vec![
            ("zero", CustomStatus::default()),
            (
                "complete",
                CustomStatus {
                    emoji: "a".into(),
                    text: "b".into(),
                    duration: "date_and_time".into(),
                    expires_at: recent,
                },
            ),
            (
                "html",
                CustomStatus {
                    emoji: "<b>".into(),
                    text: "a&b".into(),
                    duration: "date_and_time".into(),
                    expires_at: recent,
                },
            ),
        ];

        for (name, cs) in inputs {
            let mut user = User::default();
            user.set_custom_status(&cs).unwrap();
            assert_eq!(
                user.get_prop(USER_PROPS_KEY_CUSTOM_STATUS).unwrap(),
                cases[name].as_str().unwrap(),
                "case {name}"
            );
        }

        // Go's nil case stores the literal "null"; a Rust `&CustomStatus` cannot be nil, so
        // assert only that the oracle still records what we chose not to reproduce.
        assert_eq!(cases["nil"].as_str().unwrap(), "null");
    }

    #[test]
    fn clear_custom_status_empties_the_key_without_removing_it() {
        let oracle = oracle();
        let want = &oracle["clear_custom_status"];

        let mut user = User::default();
        user.clear_custom_status();

        assert_eq!(
            user.get_prop(USER_PROPS_KEY_CUSTOM_STATUS),
            Some(want["value"].as_str().unwrap())
        );
        assert!(want["key_exists"].as_bool().unwrap());
        // The key surviving is what keeps a cleared status valid rather than absent.
        assert!(user.validate_custom_status());
    }

    #[test]
    fn custom_status_is_an_alias_for_get_custom_status() {
        let mut user = User::default();
        user.set_custom_status(&CustomStatus {
            emoji: "a".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(user.custom_status(), user.get_custom_status());
    }

    /// The round trip that matters: a status set by one server must be readable by the other.
    #[test]
    fn set_then_get_round_trips_through_props() {
        let cs = CustomStatus {
            emoji: "<tada>".into(),
            text: "a&b".into(),
            duration: "date_and_time".into(),
            expires_at: go_time::parse("2026-08-14T12:00:00+05:30").unwrap(),
        };

        let mut user = User::default();
        user.set_custom_status(&cs).unwrap();
        assert_eq!(user.get_custom_status(), Some(cs));
    }
}

// --- locale validation ---------------------------------------------------------------------------

/// Port of `IsValidLocale` (user.go:1105). Closes [D-001].
///
/// # The registry is the rule
///
/// Go delegates to `golang.org/x/text/language.Parse`, which validates against the **IANA subtag
/// registry** rather than against BCP 47 syntax. `xx` is syntactically perfect and rejected,
/// because it is not a registered language; `qaa`, `mul` and `zxx` look like nonsense and are
/// accepted, because they are. There is nothing to reason about, so nothing here is reasoned.
///
/// # How the tables were built
///
/// `UserLocaleMaxLength` is 5, so the reachable input space is every string of at most five bytes
/// — 81,376,658 of them over the characters a tag can contain. The generator asks Go about every
/// one, decomposes the 234,421 it accepts into the component tables in [`crate::locale_generated`],
/// and then **re-derives all 81 million answers from those tables**, failing if a single one
/// disagrees.
///
/// That step earned its cost immediately: the first rule missed the registry's **grandfathered**
/// tags — `i-ami`, `i-hak`, `i-lux` and five others — and the verification named all sixteen
/// rather than letting them ship. They are now an exception list the generator derives from its
/// own residual, not one anybody typed out.
///
/// # Empty is valid
///
/// Go tests `locale != ""` before anything else, so an unset locale passes. That is not an
/// oversight to tidy: a user with no locale is normal.
pub fn is_valid_locale(locale: &str) -> bool {
    use crate::locale_generated::{EXCEPTIONS, LANGUAGES_2, LANGUAGES_3, REGIONS_2};

    if locale.is_empty() {
        return true;
    }

    // `len(locale)` in Go is bytes, and the check runs before `Parse`, so a six-byte tag is
    // rejected without ever consulting the registry.
    if locale.len() > USER_LOCALE_MAX_LENGTH {
        return false;
    }

    // The tables are lower-case; `language.Parse` is case-insensitive.
    let lower = crate::utils::go_to_lower(locale);
    let s = lower.as_str();
    let bytes = s.as_bytes();

    let in_table = |table: &[&str], needle: &str| table.binary_search(&needle).is_ok();

    if in_table(EXCEPTIONS, s) {
        return true;
    }
    if s == "root" {
        return true;
    }

    let is_letter = |c: u8| c.is_ascii_lowercase();
    let is_sep = |c: u8| c == b'-' || c == b'_';

    // Private use: `x` then one or more separator-delimited alphanumeric subtags.
    if bytes.len() >= 3 && bytes[0] == b'x' && is_sep(bytes[1]) {
        let rest = &bytes[2..];
        if rest.is_empty() || is_sep(rest[0]) || is_sep(rest[rest.len() - 1]) {
            return false;
        }
        let mut previous_was_sep = false;
        for &c in rest {
            if is_sep(c) {
                if previous_was_sep {
                    return false;
                }
                previous_was_sep = true;
            } else if !c.is_ascii_alphanumeric() {
                return false;
            } else {
                previous_was_sep = false;
            }
        }
        return true;
    }

    match bytes.len() {
        2 if bytes.iter().all(|&c| is_letter(c)) => in_table(LANGUAGES_2, s),
        3 if bytes.iter().all(|&c| is_letter(c)) => in_table(LANGUAGES_3, s),
        5 if is_letter(bytes[0])
            && is_letter(bytes[1])
            && is_sep(bytes[2])
            && is_letter(bytes[3])
            && is_letter(bytes[4]) =>
        {
            in_table(LANGUAGES_2, &s[..2]) && in_table(REGIONS_2, &s[3..])
        }
        _ => false,
    }
}

/// Parity tests for [`is_valid_locale`], driven by `fixtures/behaviour_locale.json`.
#[cfg(test)]
mod locale_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_locale.json")).unwrap()
    }

    #[test]
    fn max_length_matches_go() {
        assert_eq!(
            oracle()["max_length"].as_u64().unwrap() as usize,
            USER_LOCALE_MAX_LENGTH
        );
    }

    /// The hand-picked probes, including every example from [D-001]'s table.
    #[test]
    fn probes_match_go() {
        for case in oracle()["probes"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            assert_eq!(
                is_valid_locale(input),
                case["valid"].as_bool().unwrap(),
                "is_valid_locale({input:?})"
            );
        }
    }

    /// A spread through the accepted set — every 2,000th entry of the enumeration — so membership
    /// is checked against real registry data rather than only the memorable cases.
    #[test]
    fn a_sample_of_the_accepted_set_is_accepted() {
        let oracle = oracle();
        let sample = oracle["accepted_sample"].as_array().unwrap();
        assert!(sample.len() > 100, "the sample should span the set");
        for value in sample {
            let input = value.as_str().unwrap();
            assert!(
                is_valid_locale(input),
                "{input:?} is in Go's accepted set and must pass here"
            );
        }
        assert_eq!(oracle["accepted_total"].as_u64().unwrap(), 234_421);
    }

    /// The grandfathered tags, which no structural rule covers and which the first version of
    /// this port got wrong.
    #[test]
    fn the_grandfathered_tags_are_accepted() {
        let oracle = oracle();
        let exceptions = oracle["exceptions"].as_array().unwrap();
        assert_eq!(exceptions.len(), 16, "eight tags, two separator spellings");
        for value in exceptions {
            let input = value.as_str().unwrap();
            assert!(is_valid_locale(input), "grandfathered {input:?}");
        }
        // ...and the one that looks just like them and is not registered.
        assert!(!is_valid_locale("i-en"));
    }

    /// The distinction that makes a table necessary: syntax is not enough.
    #[test]
    fn syntax_is_not_the_rule() {
        // Well-formed and unregistered.
        for rejected in ["xx", "xxx", "zh-Ha", "en-1", "a-b"] {
            assert!(
                !is_valid_locale(rejected),
                "{rejected:?} should be rejected"
            );
        }
        // Odd-looking and registered.
        for accepted in ["qaa", "mul", "zxx", "und", "root"] {
            assert!(is_valid_locale(accepted), "{accepted:?} should be accepted");
        }
    }

    /// Empty passes, and the length check is on bytes and runs before the registry lookup.
    #[test]
    fn empty_passes_and_six_bytes_fails() {
        assert!(is_valid_locale(""));
        assert!(is_valid_locale("en-US"));
        assert!(
            !is_valid_locale("en-USA"),
            "six bytes, rejected before Parse"
        );
    }
}

/// Parity tests for [`User::is_valid`], driven by `fixtures/behaviour_user_is_valid.json`.
#[cfg(test)]
mod is_valid_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_user_is_valid.json"
        ))
        .unwrap()
    }

    fn runes(n: usize, c: char) -> String {
        std::iter::repeat_n(c, n).collect()
    }

    fn valid_user() -> User {
        User {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            create_at: 1_600_000_000_000,
            update_at: 1_650_000_000_000,
            username: "someuser".to_owned(),
            email: "someone@example.com".to_owned(),
            nickname: "nick".to_owned(),
            position: "position".to_owned(),
            first_name: "First".to_owned(),
            last_name: "Last".to_owned(),
            locale: "en".to_owned(),
            roles: "system_user".to_owned(),
            ..Default::default()
        }
    }

    fn user_for(name: &str) -> User {
        let mut u = valid_user();
        match name {
            "valid" => {}
            "bad_id" => u.id = "nope".to_owned(),
            "empty_id" => u.id = String::new(),
            "zero_create_at" => u.create_at = 0,
            "zero_update_at" => u.update_at = 0,
            "bad_username" => u.username = "Has Spaces".to_owned(),
            "empty_username" => u.username = String::new(),
            "empty_email" => u.email = String::new(),
            "bad_email" => u.email = "not an email".to_owned(),
            "email_at_cap" => {
                u.email = format!(
                    "{}@example.com",
                    runes(USER_EMAIL_MAX_LENGTH - "@example.com".len(), 'a')
                )
            }
            "email_over_cap" => {
                u.email = format!(
                    "{}@example.com",
                    runes(USER_EMAIL_MAX_LENGTH - "@example.com".len() + 1, 'a')
                )
            }
            "email_multibyte_over_cap_in_bytes" => {
                u.email = format!("{}@example.com", runes(USER_EMAIL_MAX_LENGTH / 2, 'é'))
            }
            "nickname_at_cap" => u.nickname = runes(USER_NICKNAME_MAX_RUNES, 'a'),
            "nickname_over_cap" => u.nickname = runes(USER_NICKNAME_MAX_RUNES + 1, 'a'),
            "nickname_multibyte_at_cap" => u.nickname = runes(USER_NICKNAME_MAX_RUNES, 'é'),
            "position_over_cap" => u.position = runes(USER_POSITION_MAX_RUNES + 1, 'a'),
            "first_name_over_cap" => u.first_name = runes(USER_FIRST_NAME_MAX_RUNES + 1, 'a'),
            "last_name_over_cap" => u.last_name = runes(USER_LAST_NAME_MAX_RUNES + 1, 'a'),
            "auth_data_over_cap" => {
                u.auth_data = Some(runes(USER_AUTH_DATA_MAX_LENGTH + 1, 'a'));
                u.auth_service = "gitlab".to_owned();
            }
            "auth_data_without_service" => {
                u.auth_data = Some("some-auth-data".to_owned());
                u.auth_service = String::new();
            }
            "auth_data_with_password" => {
                u.auth_data = Some("some-auth-data".to_owned());
                u.auth_service = "gitlab".to_owned();
                u.password = "hashed".to_owned();
            }
            "auth_data_empty_pointer" => u.auth_data = Some(String::new()),
            "auth_data_valid" => {
                u.auth_data = Some("some-auth-data".to_owned());
                u.auth_service = "gitlab".to_owned();
            }
            "password_without_auth_data" => u.password = "hashed".to_owned(),
            "bad_locale" => u.locale = "xx".to_owned(),
            "empty_locale_is_valid" => u.locale = String::new(),
            "locale_over_length" => u.locale = "en-USA".to_owned(),
            "roles_at_cap" => u.roles = runes(USER_ROLES_MAX_LENGTH, 'a'),
            "roles_over_cap" => u.roles = runes(USER_ROLES_MAX_LENGTH + 1, 'a'),
            "nil_props_skips_custom_status" => u.props = None,
            "empty_props_is_not_nil" => u.props = Some(utils::StringMap::new()),
            "props_with_bad_custom_status" => {
                let mut props = utils::StringMap::new();
                props.insert(
                    crate::custom_status::USER_PROPS_KEY_CUSTOM_STATUS.to_owned(),
                    "not json".to_owned(),
                );
                u.props = Some(props);
            }
            "timezone_over_cap" => {
                let mut tz = utils::StringMap::new();
                tz.insert(
                    "automaticTimezone".to_owned(),
                    runes(USER_TIMEZONE_MAX_RUNES, 'a'),
                );
                tz.insert("manualTimezone".to_owned(), "b".to_owned());
                tz.insert("useAutomaticTimezone".to_owned(), "true".to_owned());
                u.timezone = Some(tz);
            }
            "timezone_small_is_valid" => {
                let mut tz = utils::StringMap::new();
                tz.insert("b".to_owned(), "2".to_owned());
                tz.insert("a".to_owned(), "1".to_owned());
                u.timezone = Some(tz);
            }
            "bad_id_and_bad_email" => {
                u.id = "nope".to_owned();
                u.email = String::new();
            }
            "zero_create_at_and_bad_username" => {
                u.create_at = 0;
                u.username = "Has Spaces".to_owned();
            }
            other => panic!("unmapped corpus case: {other}"),
        }
        u
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["UserEmailMaxLength"], USER_EMAIL_MAX_LENGTH);
        assert_eq!(c["UserNicknameMaxRunes"], USER_NICKNAME_MAX_RUNES);
        assert_eq!(c["UserPositionMaxRunes"], USER_POSITION_MAX_RUNES);
        assert_eq!(c["UserFirstNameMaxRunes"], USER_FIRST_NAME_MAX_RUNES);
        assert_eq!(c["UserLastNameMaxRunes"], USER_LAST_NAME_MAX_RUNES);
        assert_eq!(c["UserAuthDataMaxLength"], USER_AUTH_DATA_MAX_LENGTH);
        assert_eq!(c["UserTimezoneMaxRunes"], USER_TIMEZONE_MAX_RUNES);
        assert_eq!(c["UserRolesMaxLength"], USER_ROLES_MAX_LENGTH);
        assert_eq!(c["UserLocaleMaxLength"], USER_LOCALE_MAX_LENGTH);
    }

    #[test]
    fn is_valid_matches_go() {
        for case in oracle()["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let got = user_for(name).is_valid();

            if case["ok"].as_bool().unwrap() {
                assert!(got.is_ok(), "{name}: expected ok, got {got:?}");
                continue;
            }

            let err = got.expect_err(&format!("{name}: expected an error"));
            assert_eq!(err.id, case["id"].as_str().unwrap(), "{name}: id");
            assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
            assert_eq!(
                err.status_code,
                case["status"].as_i64().unwrap() as i32,
                "{name}: status"
            );

            // The `auth_data` length branch interpolates a POINTER in Go, so its detail holds a
            // memory address that changes between calls. Ours holds a marker. See D-107.
            if name == "auth_data_over_cap" {
                assert!(
                    case["detailed_error"].as_str().unwrap().contains("0x"),
                    "Go's detail should still be an address; if not, D-107 can be closed"
                );
                assert_eq!(
                    err.detailed_error,
                    format!("user_id={} auth_data=<pointer>", valid_user().id)
                );
                continue;
            }

            assert_eq!(
                err.detailed_error,
                case["detailed_error"].as_str().unwrap(),
                "{name}: detailed_error"
            );
        }
    }

    /// The exemption that a tidy refactor would remove.
    #[test]
    fn a_remote_user_may_hold_an_invalid_email() {
        for case in oracle()["remote_email"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut u = valid_user();
            u.email = match name {
                "local_valid_email" | "remote_valid_email" => "someone@example.com".to_owned(),
                "local_invalid_email" | "remote_invalid_email" => "not an email".to_owned(),
                "remote_empty_email" => String::new(),
                "remote_over_cap_email" => runes(USER_EMAIL_MAX_LENGTH + 1, 'a'),
                other => panic!("unmapped: {other}"),
            };
            if name.starts_with("remote") {
                u.remote_id = Some("aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned());
            }

            assert_eq!(
                u.is_remote(),
                case["is_remote"].as_bool().unwrap(),
                "{name}: is_remote"
            );
            assert_eq!(
                u.is_valid().is_ok(),
                case["ok"].as_bool().unwrap(),
                "{name}: validity"
            );
        }

        // Stated directly: only the FORMAT check is skipped.
        let mut remote = valid_user();
        remote.remote_id = Some("aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned());
        remote.email = "not an email".to_owned();
        assert!(remote.is_valid().is_ok(), "format is skipped for remote");
        remote.email = String::new();
        assert!(remote.is_valid().is_err(), "emptiness is not skipped");
    }

    /// The timezone cap counts runes of Go's **marshalled** JSON, escaping included.
    #[test]
    fn the_timezone_cap_measures_gos_json() {
        for case in oracle()["timezone_json"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut tz = utils::StringMap::new();
            match name {
                "nil" | "empty" => {}
                "typical" => {
                    tz.insert(
                        "automaticTimezone".to_owned(),
                        "America/New_York".to_owned(),
                    );
                    tz.insert("manualTimezone".to_owned(), String::new());
                    tz.insert("useAutomaticTimezone".to_owned(), "true".to_owned());
                }
                "html_escapable" => {
                    tz.insert("a".to_owned(), "<".to_owned());
                }
                other => panic!("unmapped: {other}"),
            }

            if name == "nil" {
                // Go marshals a nil map to `null`; we never reach the branch for `None`.
                continue;
            }

            let marshalled = utils::go_json_marshal_string_map(Some(&tz));
            assert_eq!(
                marshalled,
                case["json"].as_str().unwrap(),
                "{name}: marshalled json"
            );
            assert_eq!(
                marshalled.chars().count(),
                case["rune_count"].as_u64().unwrap() as usize,
                "{name}: rune count"
            );
        }

        // The escaping is the point: `<` costs six runes, so it eats the budget six times faster.
        let mut escapable = utils::StringMap::new();
        escapable.insert("a".to_owned(), "<".to_owned());
        assert!(utils::go_json_marshal_string_map(Some(&escapable)).contains("\\u003c"));
    }

    /// Go's `%v` on a map, which the timezone branch interpolates into its detail.
    #[test]
    fn map_formatting_matches_go() {
        for case in oracle()["map_format"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut map = utils::StringMap::new();
            match name {
                "nil" | "empty" => {}
                "one" => {
                    map.insert("a".to_owned(), "1".to_owned());
                }
                "sorted" => {
                    map.insert("z".to_owned(), "26".to_owned());
                    map.insert("a".to_owned(), "1".to_owned());
                    map.insert("m".to_owned(), "13".to_owned());
                }
                "empty_value" => {
                    map.insert("a".to_owned(), String::new());
                }
                "space_in_value" => {
                    map.insert("a".to_owned(), "one two".to_owned());
                }
                other => panic!("unmapped: {other}"),
            }
            assert_eq!(
                utils::go_format_string_map(&map),
                case["rendered"].as_str().unwrap(),
                "map format for {name}"
            );
        }
    }

    /// A nil `Props` skips the custom-status check; an empty-but-present map does not.
    #[test]
    fn props_gates_the_custom_status_check() {
        let mut none = valid_user();
        none.props = None;
        assert!(none.is_valid().is_ok());

        let mut empty = valid_user();
        empty.props = Some(utils::StringMap::new());
        assert!(empty.is_valid().is_ok(), "an empty map still validates");

        let mut bad = valid_user();
        let mut props = utils::StringMap::new();
        props.insert(
            crate::custom_status::USER_PROPS_KEY_CUSTOM_STATUS.to_owned(),
            "not json".to_owned(),
        );
        bad.props = Some(props);
        assert!(bad.is_valid().is_err());
    }
}
