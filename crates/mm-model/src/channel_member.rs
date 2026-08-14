//! Port of `model/channel_member.go` (channel_member.go:1–297).
//!
//! The notify-props validator is the interesting part: whether a *missing* key is an error
//! depends on which key it is and on `allow_missing_fields`, and two of its error details are
//! wrong in Go in ways clients already depend on. Every branch is pinned by
//! `fixtures/behaviour_channel_member.json`.
//!
//! # Deliberately not translated here
//!
//! - `Auditable` is an audit-log projection; it follows the audit layer, as with `Channel`.
//! - `ChannelMembers` and `ChannelMembersWithTeamData` are Go slice aliases with no methods;
//!   they are plain type aliases here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::user::{
    DESKTOP_NOTIFY_PROP, EMAIL_NOTIFY_PROP, MARK_UNREAD_NOTIFY_PROP, PUSH_NOTIFY_PROP,
    USER_ROLES_MAX_LENGTH,
};
use crate::utils::{
    AppError, AppResult, StringMap, get_millis, go_json_marshal_string_map, is_valid_id,
};

// ---------------------------------------------------------------------------
// Constants (channel_member.go:13-27)
// ---------------------------------------------------------------------------

pub const CHANNEL_NOTIFY_DEFAULT: &str = "default";
pub const CHANNEL_NOTIFY_ALL: &str = "all";
pub const CHANNEL_NOTIFY_MENTION: &str = "mention";
pub const CHANNEL_NOTIFY_NONE: &str = "none";
pub const CHANNEL_MARK_UNREAD_ALL: &str = "all";
pub const CHANNEL_MARK_UNREAD_MENTION: &str = "mention";
pub const IGNORE_CHANNEL_MENTIONS_DEFAULT: &str = "default";
pub const IGNORE_CHANNEL_MENTIONS_OFF: &str = "off";
pub const IGNORE_CHANNEL_MENTIONS_ON: &str = "on";
pub const IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP: &str = "ignore_channel_mentions";
pub const CHANNEL_AUTO_FOLLOW_THREADS_OFF: &str = "off";
pub const CHANNEL_AUTO_FOLLOW_THREADS_ON: &str = "on";
pub const CHANNEL_AUTO_FOLLOW_THREADS: &str = "channel_auto_follow_threads";
pub const CHANNEL_MEMBER_NOTIFY_PROPS_MAX_RUNES: usize = 800_000;

// ---------------------------------------------------------------------------
// Unread counters
// ---------------------------------------------------------------------------

/// Port of `model.ChannelUnread` (channel_member.go:30).
///
/// `notify_props` carries `json:"-"`: it is populated by the store and consumed by the
/// notification logic, never sent to a client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelUnread {
    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "msg_count")]
    pub msg_count: i64,

    #[serde(rename = "mention_count")]
    pub mention_count: i64,

    #[serde(rename = "mention_count_root")]
    pub mention_count_root: i64,

    #[serde(rename = "urgent_mention_count")]
    pub urgent_mention_count: i64,

    #[serde(rename = "msg_count_root")]
    pub msg_count_root: i64,

    #[serde(skip)]
    pub notify_props: Option<StringMap>,
}

/// Port of `model.ChannelUnreadAt` (channel_member.go:41).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelUnreadAt {
    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "msg_count")]
    pub msg_count: i64,

    #[serde(rename = "mention_count")]
    pub mention_count: i64,

    #[serde(rename = "mention_count_root")]
    pub mention_count_root: i64,

    #[serde(rename = "urgent_mention_count")]
    pub urgent_mention_count: i64,

    #[serde(rename = "msg_count_root")]
    pub msg_count_root: i64,

    #[serde(rename = "last_viewed_at")]
    pub last_viewed_at: i64,

    #[serde(skip)]
    pub notify_props: Option<StringMap>,
}

// ---------------------------------------------------------------------------
// ChannelMember
// ---------------------------------------------------------------------------

/// Port of `model.ChannelMember` (channel_member.go:54).
///
/// `notify_props` has no `omitempty`, so a nil map serialises as `null` with the key present.
/// Nil is also reachable at runtime: [`ChannelMember::set_channel_muted`] writes into the map
/// and **panics in Go** when it is nil (see D-018).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMember {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "roles")]
    pub roles: String,

    #[serde(rename = "last_viewed_at")]
    pub last_viewed_at: i64,

    #[serde(rename = "msg_count")]
    pub msg_count: i64,

    #[serde(rename = "mention_count")]
    pub mention_count: i64,

    #[serde(rename = "mention_count_root")]
    pub mention_count_root: i64,

    #[serde(rename = "urgent_mention_count")]
    pub urgent_mention_count: i64,

    #[serde(rename = "msg_count_root")]
    pub msg_count_root: i64,

    #[serde(rename = "notify_props")]
    pub notify_props: Option<StringMap>,

    #[serde(rename = "last_update_at")]
    pub last_update_at: i64,

    #[serde(rename = "scheme_guest")]
    pub scheme_guest: bool,

    #[serde(rename = "scheme_user")]
    pub scheme_user: bool,

    #[serde(rename = "scheme_admin")]
    pub scheme_admin: bool,

    #[serde(rename = "explicit_roles")]
    pub explicit_roles: String,

    #[serde(rename = "autotranslation_disabled")]
    pub auto_translation_disabled: bool,
}

impl ChannelMember {
    /// Port of `(*ChannelMember).SanitizeForCurrentUser` (channel_member.go:93).
    ///
    /// Both timestamps become **`-1`**, not `0`, for anyone but the member themselves. That
    /// sentinel reaches the client, same as `TeamMember::sanitize_role_data`.
    pub fn sanitize_for_current_user(&mut self, current_user_id: &str) {
        if self.user_id != current_user_id {
            self.last_viewed_at = -1;
            self.last_update_at = -1;
        }
    }

    /// Port of `(*ChannelMember).IsValid` (channel_member.go:126).
    ///
    /// Note the notify-props check runs with `allow_missing_fields = false`, so a member whose
    /// `notify_props` is nil or empty is **invalid** — the missing `desktop` key is reported as
    /// `notify_level=` with an empty value.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.channel_id) {
            return Err(error("channel_id", None, String::new()));
        }
        if !is_valid_id(&self.user_id) {
            return Err(error("user_id", None, String::new()));
        }

        is_channel_member_notify_props_valid(self.notify_props.as_ref(), false)?;

        if self.roles.len() > USER_ROLES_MAX_LENGTH {
            let params = HashMap::from([(
                "Limit".to_string(),
                serde_json::Value::from(USER_ROLES_MAX_LENGTH),
            )]);
            return Err(error("roles_limit", Some(params), String::new()));
        }

        Ok(())
    }

    /// Port of `(*ChannelMember).PreSave` (channel_member.go:194). Identical to `PreUpdate`.
    pub fn pre_save(&mut self) {
        self.last_update_at = get_millis();
    }

    /// Port of `(*ChannelMember).PreUpdate` (channel_member.go:198).
    pub fn pre_update(&mut self) {
        self.last_update_at = get_millis();
    }

    /// Port of `(*ChannelMember).GetRoles` (channel_member.go:202). `strings.Fields` splits on
    /// runs of any whitespace, tabs and NBSP included.
    pub fn get_roles(&self) -> Vec<&str> {
        self.roles.split_whitespace().collect()
    }

    /// Port of `(*ChannelMember).SetChannelMuted` (channel_member.go:206).
    ///
    /// **The `muted` argument is ignored.** Go branches on `IsChannelMuted()` and writes the
    /// opposite value, so this is a toggle wearing a setter's name: `set_channel_muted(false)`
    /// on an unmuted channel mutes it. Verified against Go across every starting value.
    /// Reproduced as-is — see D-019 before "fixing" it.
    ///
    /// One deliberate divergence: Go panics on a nil `NotifyProps` (assignment to a nil map).
    /// This creates the map instead.
    pub fn set_channel_muted(&mut self, _muted: bool) {
        let value = if self.is_channel_muted() {
            CHANNEL_MARK_UNREAD_ALL
        } else {
            CHANNEL_MARK_UNREAD_MENTION
        };
        self.notify_props
            .get_or_insert_with(StringMap::new)
            .insert(MARK_UNREAD_NOTIFY_PROP.to_string(), value.to_string());
    }

    /// Port of `(*ChannelMember).IsChannelMuted` (channel_member.go:214).
    ///
    /// A missing or nil `notify_props` reads as the zero value, so an absent `mark_unread` is
    /// "not muted".
    pub fn is_channel_muted(&self) -> bool {
        self.notify_props
            .as_ref()
            .and_then(|props| props.get(MARK_UNREAD_NOTIFY_PROP))
            .map(String::as_str)
            == Some(CHANNEL_MARK_UNREAD_MENTION)
    }
}

/// Every error in this file reports `Where = "ChannelMember.IsValid"`, including the ones
/// raised by the free function `is_channel_member_notify_props_valid`.
fn error(
    field: &str,
    params: Option<HashMap<String, serde_json::Value>>,
    details: String,
) -> Box<AppError> {
    Box::new(AppError::new(
        "ChannelMember.IsValid",
        format!("model.channel_member.is_valid.{field}.app_error"),
        params,
        details,
        400,
    ))
}

// ---------------------------------------------------------------------------
// Notify-props validation
// ---------------------------------------------------------------------------

/// Port of `model.IsChannelMemberNotifyPropsValid` (channel_member.go:150).
///
/// The two shapes matter and are easy to conflate:
///
/// - `desktop` and `mark_unread` are checked when present **or** when `allow_missing_fields`
///   is false. With the flag off, omitting either is an error in its own right.
/// - `push`, `email`, `ignore_channel_mentions` and `channel_auto_follow_threads` are checked
///   only when present. Omitting them is always fine.
///
/// Two details are wrong in Go and are reproduced verbatim because clients parse them:
/// the email failure reports `push_notification_level=`, not `email=`; and every detail
/// interpolates the offending value even when the failure was the length check.
///
/// A nil map behaves exactly like an empty one — Go's map read on nil yields `("", false)`.
pub fn is_channel_member_notify_props_valid(
    notify_props: Option<&StringMap>,
    allow_missing_fields: bool,
) -> AppResult {
    let get = |key: &str| {
        notify_props
            .and_then(|props| props.get(key))
            .map(String::as_str)
    };

    if let Some(level) = get(DESKTOP_NOTIFY_PROP) {
        if level.len() > 20 || !is_channel_notify_level_valid(level) {
            return Err(error("notify_level", None, format!("notify_level={level}")));
        }
    } else if !allow_missing_fields {
        return Err(error("notify_level", None, "notify_level=".to_string()));
    }

    if let Some(level) = get(MARK_UNREAD_NOTIFY_PROP) {
        if level.len() > 20 || !is_channel_mark_unread_level_valid(level) {
            return Err(error(
                "unread_level",
                None,
                format!("mark_unread_level={level}"),
            ));
        }
    } else if !allow_missing_fields {
        return Err(error(
            "unread_level",
            None,
            "mark_unread_level=".to_string(),
        ));
    }

    if let Some(level) = get(PUSH_NOTIFY_PROP)
        && (level.len() > 20 || !is_channel_notify_level_valid(level))
    {
        return Err(error(
            "push_level",
            None,
            format!("push_notification_level={level}"),
        ));
    }

    if let Some(send_email) = get(EMAIL_NOTIFY_PROP)
        && (send_email.len() > 20 || !is_send_email_valid(send_email))
    {
        // Go's detail label is wrong here — it says push_notification_level. Kept.
        return Err(error(
            "email_value",
            None,
            format!("push_notification_level={send_email}"),
        ));
    }

    if let Some(ignore) = get(IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP)
        && (ignore.len() > 40 || !is_ignore_channel_mentions_valid(ignore))
    {
        return Err(error(
            "ignore_channel_mentions_value",
            None,
            format!("ignore_channel_mentions={ignore}"),
        ));
    }

    if let Some(auto_follow) = get(CHANNEL_AUTO_FOLLOW_THREADS)
        && (auto_follow.len() > 3 || !is_channel_auto_follow_threads_valid(auto_follow))
    {
        return Err(error(
            "channel_auto_follow_threads_value",
            None,
            format!("channel_auto_follow_threads={auto_follow}"),
        ));
    }

    // The size cap is measured on Go's own JSON encoding of the map, which escapes more than
    // serde_json does — see `go_json_marshal_string_map`.
    let encoded = go_json_marshal_string_map(notify_props);
    let runes = encoded.chars().count();
    if runes > CHANNEL_MEMBER_NOTIFY_PROPS_MAX_RUNES {
        return Err(error("notify_props", None, format!("length={runes}")));
    }

    Ok(())
}

/// Port of `model.IsChannelNotifyLevelValid` (channel_member.go:218).
pub fn is_channel_notify_level_valid(notify_level: &str) -> bool {
    matches!(
        notify_level,
        CHANNEL_NOTIFY_DEFAULT | CHANNEL_NOTIFY_ALL | CHANNEL_NOTIFY_MENTION | CHANNEL_NOTIFY_NONE
    )
}

/// Port of `model.IsChannelMarkUnreadLevelValid` (channel_member.go:225).
///
/// `"default"` is **not** accepted here, unlike every other notify level.
pub fn is_channel_mark_unread_level_valid(mark_unread_level: &str) -> bool {
    mark_unread_level == CHANNEL_MARK_UNREAD_ALL || mark_unread_level == CHANNEL_MARK_UNREAD_MENTION
}

/// Port of `model.IsSendEmailValid` (channel_member.go:229).
///
/// Accepts `"default"`, `"true"` and `"false"` — not the notify levels.
pub fn is_send_email_valid(send_email: &str) -> bool {
    send_email == CHANNEL_NOTIFY_DEFAULT || send_email == "true" || send_email == "false"
}

/// Port of `model.IsIgnoreChannelMentionsValid` (channel_member.go:233).
pub fn is_ignore_channel_mentions_valid(ignore_channel_mentions: &str) -> bool {
    matches!(
        ignore_channel_mentions,
        IGNORE_CHANNEL_MENTIONS_ON | IGNORE_CHANNEL_MENTIONS_OFF | IGNORE_CHANNEL_MENTIONS_DEFAULT
    )
}

/// Port of `model.IsChannelAutoFollowThreadsValid` (channel_member.go:237).
///
/// Only `"on"` and `"off"`; `"default"` is rejected.
pub fn is_channel_auto_follow_threads_valid(channel_auto_follow_threads: &str) -> bool {
    channel_auto_follow_threads == CHANNEL_AUTO_FOLLOW_THREADS_ON
        || channel_auto_follow_threads == CHANNEL_AUTO_FOLLOW_THREADS_OFF
}

/// Port of `model.GetDefaultChannelNotifyProps` (channel_member.go:241).
///
/// These six keys are exactly the set `is_channel_member_notify_props_valid` inspects.
pub fn get_default_channel_notify_props() -> StringMap {
    StringMap::from([
        (
            DESKTOP_NOTIFY_PROP.to_string(),
            CHANNEL_NOTIFY_DEFAULT.to_string(),
        ),
        (
            MARK_UNREAD_NOTIFY_PROP.to_string(),
            CHANNEL_MARK_UNREAD_ALL.to_string(),
        ),
        (
            PUSH_NOTIFY_PROP.to_string(),
            CHANNEL_NOTIFY_DEFAULT.to_string(),
        ),
        (
            EMAIL_NOTIFY_PROP.to_string(),
            CHANNEL_NOTIFY_DEFAULT.to_string(),
        ),
        (
            IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP.to_string(),
            IGNORE_CHANNEL_MENTIONS_DEFAULT.to_string(),
        ),
        (
            CHANNEL_AUTO_FOLLOW_THREADS.to_string(),
            CHANNEL_AUTO_FOLLOW_THREADS_OFF.to_string(),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Companion wire types
// ---------------------------------------------------------------------------

/// Port of `model.ChannelMemberWithTeamData` (channel_member.go:104).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberWithTeamData {
    #[serde(flatten)]
    pub channel_member: ChannelMember,

    #[serde(rename = "team_display_name")]
    pub team_display_name: String,

    #[serde(rename = "team_name")]
    pub team_name: String,

    #[serde(rename = "team_update_at")]
    pub team_update_at: i64,
}

/// Port of `model.ChannelMembers` (channel_member.go:111).
pub type ChannelMembers = Vec<ChannelMember>;

/// Port of `model.ChannelMembersWithTeamData` (channel_member.go:113).
pub type ChannelMembersWithTeamData = Vec<ChannelMemberWithTeamData>;

/// Port of `model.ChannelMemberForExport` (channel_member.go:115).
///
/// `ChannelName` and `Username` have no json tag, so Go marshals them under the Go field names
/// verbatim — same trap as `ChannelForExport` and `TeamForExport`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberForExport {
    #[serde(flatten)]
    pub channel_member: ChannelMember,

    #[serde(rename = "ChannelName")]
    pub channel_name: String,

    #[serde(rename = "Username")]
    pub username: String,
}

/// Port of `model.ChannelMemberIdentifier` (channel_member.go:253).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberIdentifier {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,
}

/// Port of `model.SetChannelMembersRequest` (channel_member.go:259) — the bulk
/// set-channel-members request body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChannelMembersRequest {
    /// The complete desired membership. Users here and in `channel_admins` are the final set.
    #[serde(rename = "members")]
    pub members: Option<Vec<String>>,

    /// Go's `*[]string`, and the nil/empty distinction is load-bearing: `None` (wire `null`)
    /// preserves existing admin roles, while `Some(vec![])` (wire `[]`) sets them
    /// declaratively and therefore **demotes every current admin**.
    #[serde(rename = "channel_admins")]
    pub channel_admins: Option<Vec<String>>,
}

/// Port of `model.SetChannelMembersResponse` (channel_member.go:272) — one batch of results,
/// streamed as NDJSON lines.
///
/// `added` and `removed` have no `omitempty`, so a nil slice is `null` and the key stays;
/// the other three are dropped when nil or empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChannelMembersResponse {
    #[serde(rename = "added")]
    pub added: Option<Vec<String>>,

    #[serde(rename = "removed")]
    pub removed: Option<Vec<String>>,

    #[serde(rename = "promoted", default, skip_serializing_if = "Vec::is_empty")]
    pub promoted: Vec<String>,

    #[serde(rename = "demoted", default, skip_serializing_if = "Vec::is_empty")]
    pub demoted: Vec<String>,

    #[serde(rename = "errors", default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<SetChannelMembersError>,
}

/// Port of `model.SetChannelMembersError` (channel_member.go:292).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChannelMembersError {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "error")]
    pub error: String,
}

/// Port of `model.ChannelMemberCursor` (channel_member.go:120). No json tags in Go, so it
/// never crosses the wire; it is the store's pagination cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelMemberCursor {
    /// When `-1`, `from_channel_id` is used as the cursor instead.
    pub page: i64,
    pub per_page: i64,
    pub from_channel_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;

    fn fixture_member() -> ChannelMember {
        serde_json::from_str(include_str!("../../../fixtures/channel_member.json")).unwrap()
    }

    fn valid_member() -> ChannelMember {
        ChannelMember {
            channel_id: utils::new_id(),
            user_id: utils::new_id(),
            roles: "channel_user".into(),
            notify_props: Some(get_default_channel_notify_props()),
            ..Default::default()
        }
    }

    macro_rules! round_trip {
        ($name:ident, $ty:ty, $file:literal) => {
            #[test]
            fn $name() {
                let go = include_str!(concat!("../../../fixtures/", $file));
                let parsed: $ty = serde_json::from_str(go).unwrap();
                let round_tripped = serde_json::to_value(&parsed).unwrap();
                let expected: serde_json::Value = serde_json::from_str(go).unwrap();
                assert_eq!(round_tripped, expected);
            }
        };
    }

    round_trip!(
        channel_member_matches_go,
        ChannelMember,
        "channel_member.json"
    );
    round_trip!(
        channel_unread_matches_go,
        ChannelUnread,
        "channel_unread.json"
    );
    round_trip!(
        channel_unread_at_matches_go,
        ChannelUnreadAt,
        "channel_unread_at.json"
    );
    round_trip!(
        member_with_team_data_matches_go,
        ChannelMemberWithTeamData,
        "channel_member_with_team_data.json"
    );
    round_trip!(
        member_for_export_matches_go,
        ChannelMemberForExport,
        "channel_member_for_export.json"
    );
    round_trip!(
        member_identifier_matches_go,
        ChannelMemberIdentifier,
        "channel_member_identifier.json"
    );
    round_trip!(
        set_members_request_matches_go,
        SetChannelMembersRequest,
        "set_channel_members_request.json"
    );
    round_trip!(
        set_members_response_matches_go,
        SetChannelMembersResponse,
        "set_channel_members_response.json"
    );
    round_trip!(
        set_members_error_matches_go,
        SetChannelMembersError,
        "set_channel_members_error.json"
    );

    #[test]
    fn notify_props_is_null_not_omitted_when_nil() {
        let value = serde_json::to_value(ChannelMember::default()).unwrap();
        assert!(value.as_object().unwrap().contains_key("notify_props"));
        assert!(value["notify_props"].is_null());
    }

    #[test]
    fn unread_notify_props_never_reaches_the_wire() {
        // json:"-" in Go.
        let unread = ChannelUnread {
            notify_props: Some(get_default_channel_notify_props()),
            ..Default::default()
        };
        let value = serde_json::to_value(&unread).unwrap();
        assert!(!value.as_object().unwrap().contains_key("notify_props"));
    }

    #[test]
    fn set_channel_members_response_omits_only_the_omitempty_fields() {
        let value = serde_json::to_value(SetChannelMembersResponse::default()).unwrap();
        let object = value.as_object().unwrap();
        for key in ["added", "removed"] {
            assert!(object.contains_key(key), "{key} must be present");
            assert!(value[key].is_null());
        }
        for key in ["promoted", "demoted", "errors"] {
            assert!(
                !object.contains_key(key),
                "{key} must be dropped when empty"
            );
        }
    }

    #[test]
    fn channel_admins_distinguishes_null_from_empty() {
        // null preserves existing admins; [] demotes everyone.
        let preserve = SetChannelMembersRequest {
            channel_admins: None,
            ..Default::default()
        };
        let declare = SetChannelMembersRequest {
            channel_admins: Some(Vec::new()),
            ..Default::default()
        };
        assert!(serde_json::to_value(&preserve).unwrap()["channel_admins"].is_null());
        assert_eq!(
            serde_json::to_value(&declare).unwrap()["channel_admins"],
            serde_json::json!([])
        );
    }

    #[test]
    fn member_for_export_keeps_gos_untagged_field_names() {
        let value = serde_json::to_value(ChannelMemberForExport::default()).unwrap();
        let object = value.as_object().unwrap();
        assert!(object.contains_key("ChannelName"));
        assert!(object.contains_key("Username"));
        assert!(!object.contains_key("channel_name"));
    }

    #[test]
    fn fixture_member_is_valid() {
        // The generator pins notify_props to Go's defaults, so the fixture is a valid member.
        assert!(fixture_member().is_valid().is_ok());
    }

    #[test]
    fn is_valid_rejects_a_member_with_no_notify_props() {
        // allow_missing_fields is false, so an absent desktop prop is itself the error.
        let mut member = valid_member();
        member.notify_props = None;
        let err = member.is_valid().unwrap_err();
        assert_eq!(
            err.id,
            "model.channel_member.is_valid.notify_level.app_error"
        );
        assert_eq!(err.detailed_error, "notify_level=");
        assert_eq!(err.status_code, 400);
    }

    #[test]
    fn roles_limit_error_carries_the_limit_param() {
        let mut member = valid_member();
        member.roles = "a".repeat(USER_ROLES_MAX_LENGTH + 1);
        let err = member.is_valid().unwrap_err();
        assert_eq!(
            err.id,
            "model.channel_member.is_valid.roles_limit.app_error"
        );
        assert_eq!(err.params.as_ref().unwrap()["Limit"], 256);
    }

    #[test]
    fn email_failure_reports_gos_wrong_label() {
        let mut props = get_default_channel_notify_props();
        props.insert(EMAIL_NOTIFY_PROP.into(), "all".into());
        let err = is_channel_member_notify_props_valid(Some(&props), false).unwrap_err();
        assert_eq!(
            err.id,
            "model.channel_member.is_valid.email_value.app_error"
        );
        // Go says push_notification_level for an email failure. Not a typo here.
        assert_eq!(err.detailed_error, "push_notification_level=all");
    }

    #[test]
    fn pre_save_and_pre_update_both_only_stamp_last_update_at() {
        let mut member = valid_member();
        member.last_viewed_at = 42;
        member.pre_save();
        assert!(member.last_update_at > 0);
        assert_eq!(member.last_viewed_at, 42);

        let stamped = member.last_update_at;
        member.last_update_at = 0;
        member.pre_update();
        assert!(member.last_update_at >= stamped);
    }

    #[test]
    fn set_channel_muted_on_nil_props_creates_the_map_instead_of_panicking() {
        // Go panics here (assignment to a nil map).
        let mut member = ChannelMember::default();
        member.set_channel_muted(true);
        assert!(member.is_channel_muted());
        assert_eq!(
            member.notify_props.unwrap()[MARK_UNREAD_NOTIFY_PROP],
            CHANNEL_MARK_UNREAD_MENTION
        );
    }

    #[test]
    fn default_notify_props_pass_validation_with_either_flag() {
        let props = get_default_channel_notify_props();
        assert!(is_channel_member_notify_props_valid(Some(&props), false).is_ok());
        assert!(is_channel_member_notify_props_valid(Some(&props), true).is_ok());
    }

    #[test]
    fn missing_optional_props_are_only_an_error_for_desktop_and_mark_unread() {
        let mut props = get_default_channel_notify_props();
        props.remove(PUSH_NOTIFY_PROP);
        props.remove(EMAIL_NOTIFY_PROP);
        props.remove(IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP);
        props.remove(CHANNEL_AUTO_FOLLOW_THREADS);
        assert!(is_channel_member_notify_props_valid(Some(&props), false).is_ok());

        props.remove(DESKTOP_NOTIFY_PROP);
        assert!(is_channel_member_notify_props_valid(Some(&props), false).is_err());
        assert!(is_channel_member_notify_props_valid(Some(&props), true).is_ok());
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_member.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal_string_map;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_member.json"
        ))
        .unwrap()
    }

    fn props_from(value: &Value) -> Option<StringMap> {
        value.as_object().map(|object| {
            object
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
    }

    fn check_levels(key: &str, predicate: impl Fn(&str) -> bool) {
        let oracle = oracle();
        let cases = oracle[key].as_object().unwrap();
        assert!(!cases.is_empty(), "{key} corpus is empty");
        for (input, want) in cases {
            assert_eq!(
                predicate(input),
                want.as_bool().unwrap(),
                "{key}({input:?})"
            );
        }
    }

    #[test]
    fn level_validators_match_go() {
        check_levels("is_channel_notify_level", is_channel_notify_level_valid);
        check_levels(
            "is_channel_mark_unread_level",
            is_channel_mark_unread_level_valid,
        );
        check_levels("is_send_email", is_send_email_valid);
        check_levels(
            "is_ignore_channel_mentions",
            is_ignore_channel_mentions_valid,
        );
        check_levels(
            "is_channel_auto_follow_thread",
            is_channel_auto_follow_threads_valid,
        );
    }

    #[test]
    fn default_notify_props_match_go() {
        let oracle = oracle();
        let want = props_from(&oracle["default_channel_notify_props"]).unwrap();
        assert_eq!(get_default_channel_notify_props(), want);
    }

    #[test]
    fn notify_props_validation_matches_go() {
        let oracle = oracle();
        let cases = oracle["notify_props_valid"].as_array().unwrap();
        assert!(cases.len() > 50, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let allow = case["allow_missing_fields"].as_bool().unwrap();
            let props = props_from(&case["props"]);
            let (got_id, got_detail) =
                match is_channel_member_notify_props_valid(props.as_ref(), allow) {
                    Ok(()) => (String::new(), String::new()),
                    Err(err) => (err.id.clone(), err.detailed_error.clone()),
                };
            assert_eq!(
                got_id,
                case["error_id"].as_str().unwrap(),
                "{name} (allow_missing_fields={allow})"
            );
            assert_eq!(
                got_detail,
                case["detailed"].as_str().unwrap(),
                "{name} (allow_missing_fields={allow}) detailed_error"
            );
        }
    }

    #[test]
    fn to_json_matches_go_byte_for_byte() {
        let oracle = oracle();
        let cases = oracle["to_json_string_map"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let props = props_from(&case["props"]);
            let encoded = go_json_marshal_string_map(props.as_ref());
            assert_eq!(encoded, case["encoded"].as_str().unwrap(), "ToJSON({name})");
            assert_eq!(
                encoded.chars().count() as u64,
                case["rune_count"].as_u64().unwrap(),
                "ToJSON({name}) rune count"
            );
            assert_eq!(
                encoded.len() as u64,
                case["bytes"].as_u64().unwrap(),
                "ToJSON({name}) byte count"
            );
        }
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_member_is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let member: ChannelMember = serde_json::from_value(case["member"].clone()).unwrap();
            let (got_id, got_detail) = match member.is_valid() {
                Ok(()) => (String::new(), String::new()),
                Err(err) => (err.id.clone(), err.detailed_error.clone()),
            };
            assert_eq!(
                got_id,
                case["error_id"].as_str().unwrap(),
                "IsValid({name})"
            );
            assert_eq!(
                got_detail,
                case["detailed"].as_str().unwrap(),
                "IsValid({name}) detailed_error"
            );
        }
    }

    #[test]
    fn set_channel_muted_matches_go_and_ignores_its_argument() {
        let oracle = oracle();
        let cases = oracle["set_channel_muted"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let start = case["start"].as_str().unwrap();
            let arg = case["arg"].as_bool().unwrap();

            let mut props = StringMap::new();
            if start != "<absent>" {
                props.insert(MARK_UNREAD_NOTIFY_PROP.to_string(), start.to_string());
            }
            let mut member = ChannelMember {
                notify_props: Some(props),
                ..Default::default()
            };

            assert_eq!(
                member.is_channel_muted(),
                case["muted_before"].as_bool().unwrap(),
                "IsChannelMuted(start={start:?})"
            );
            member.set_channel_muted(arg);
            assert_eq!(
                member.notify_props.as_ref().unwrap()[MARK_UNREAD_NOTIFY_PROP],
                case["mark_unread"].as_str().unwrap(),
                "SetChannelMuted(start={start:?}, arg={arg})"
            );
            assert_eq!(
                member.is_channel_muted(),
                case["muted_after"].as_bool().unwrap(),
                "IsChannelMuted after (start={start:?}, arg={arg})"
            );
        }
    }

    #[test]
    fn sanitize_for_current_user_matches_go() {
        let oracle = oracle();
        let cases = oracle["sanitize_for_current_user"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let mut member = ChannelMember {
                user_id: case["member_user_id"].as_str().unwrap().to_string(),
                last_viewed_at: 1_700_557_221_000,
                last_update_at: 1_707_722_148_000,
                ..Default::default()
            };
            let current = case["current_user"].as_str().unwrap();
            member.sanitize_for_current_user(current);
            assert_eq!(
                member.last_viewed_at,
                case["last_viewed_at"].as_i64().unwrap(),
                "current={current:?} last_viewed_at"
            );
            assert_eq!(
                member.last_update_at,
                case["last_update_at"].as_i64().unwrap(),
                "current={current:?} last_update_at"
            );
        }
    }

    #[test]
    fn get_roles_matches_go() {
        let oracle = oracle();
        let cases = oracle["member_get_roles"].as_object().unwrap();
        assert!(!cases.is_empty());

        for (input, want) in cases {
            let member = ChannelMember {
                roles: input.clone(),
                ..Default::default()
            };
            let want: Vec<&str> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(member.get_roles(), want, "GetRoles({input:?})");
        }
    }
}
