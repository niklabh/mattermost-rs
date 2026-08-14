//! Port of `model/channel.go` (channel.go:1–635).
//!
//! Every branch of `IsValid`, `IsValidBoard`, `Patch`, `PreSave` and the DM/GM naming helpers
//! is measured against Go via `fixtures/behaviour_channel.json` rather than reasoned about.
//! Several of the answers are counter-intuitive and are called out at their definitions.
//!
//! # Deliberately not translated here
//!
//! - `ChannelBannerInfo::Scan`/`Value` are `database/sql` plumbing and belong to `mm-store`
//!   (D-013). `Value()` returning SQL `NULL` for an all-nil struct is a real semantic that
//!   must survive the move.
//! - `Auditable`/`LogClone` are audit-log projections; they follow the audit layer, as with
//!   `Team`.
//! - `ChannelOption`/`WithID` are Go's functional-options idiom. `Channel { id, ..default() }`
//!   is the Rust equivalent and needs no code.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::channel_list::ChannelListWithTeamData;
use crate::channel_member::ChannelMemberForExport;
use crate::team::ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP;
use crate::user::User;
use crate::utils::{
    AppError, AppResult, StringInterface, get_millis, is_valid_id, is_valid_simple_alpha_num,
    limit_bytes, new_id, sanitize_unicode,
};

// ---------------------------------------------------------------------------
// Constants (channel.go:26-50)
// ---------------------------------------------------------------------------

pub const CHANNEL_TYPE_OPEN: &str = "O";
pub const CHANNEL_TYPE_PRIVATE: &str = "P";
pub const CHANNEL_TYPE_DIRECT: &str = "D";
pub const CHANNEL_TYPE_GROUP: &str = "G";
pub const CHANNEL_TYPE_SPACE: &str = "S";
pub const CHANNEL_TYPE_OPEN_BOARD: &str = "BO";
pub const CHANNEL_TYPE_PRIVATE_BOARD: &str = "BP";

pub const CHANNEL_PROPS_BOARD_LINKED_PROPERTIES: &str = "board:linked_properties";

pub const CHANNEL_GROUP_MAX_USERS: usize = 8;
pub const CHANNEL_GROUP_MIN_USERS: usize = 3;
pub const DEFAULT_CHANNEL_NAME: &str = "town-square";
pub const CHANNEL_DISPLAY_NAME_MAX_RUNES: usize = 64;
pub const CHANNEL_NAME_MIN_LENGTH: usize = 1;
pub const CHANNEL_NAME_MAX_LENGTH: usize = 64;
pub const CHANNEL_HEADER_MAX_RUNES: usize = 1024;
pub const CHANNEL_PURPOSE_MAX_RUNES: usize = 250;
pub const CHANNEL_CACHE_SIZE: usize = 25000;
pub const CHANNEL_BANNER_INFO_MAX_LENGTH: usize = 1024;

pub const CHANNEL_SORT_BY_USERNAME: &str = "username";
pub const CHANNEL_SORT_BY_STATUS: &str = "status";

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// channel.go:22. Accepts `#RGB` and `#RRGGBB`, either case.
static CHANNEL_HEX_COLOR_REGEX: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$").ok());

/// channel.go:294 — the shape `GetGroupNameFromUserIds` produces (40 lowercase hex digits).
/// `IsValid` rejects any non-DM/GM channel whose name has that shape.
static GM_NAME_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new("^[a-f0-9]{40}$").ok());

/// Fails closed if a pattern is malformed, rather than panicking — see the same helper in
/// `utils.rs`. Both patterns here are compile-time constants and are covered by tests.
fn matches(re: &LazyLock<Option<Regex>>, s: &str) -> bool {
    re.as_ref().is_some_and(|re| re.is_match(s))
}

/// Port of `model.IsValidChannelIdentifier` (utils.go:705).
///
/// Lives here rather than in `utils.rs` because the length half of the check is
/// `ChannelNameMinLength`, which `channel.go` owns. The regex already requires at least one
/// character, so the length term is redundant in Go too; it is kept for fidelity.
pub fn is_valid_channel_identifier(s: &str) -> bool {
    is_valid_simple_alpha_num(s) && s.len() >= CHANNEL_NAME_MIN_LENGTH
}

fn bool_map_is_empty(m: &Option<HashMap<String, bool>>) -> bool {
    m.as_ref().is_none_or(HashMap::is_empty)
}

// ---------------------------------------------------------------------------
// ChannelBannerInfo
// ---------------------------------------------------------------------------

/// Port of `model.ChannelBannerInfo` (channel.go:53).
///
/// None of the three fields carries `omitempty`, so all three keys are always present and a
/// nil pointer serialises as `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBannerInfo {
    #[serde(rename = "enabled")]
    pub enabled: Option<bool>,

    #[serde(rename = "text")]
    pub text: Option<String>,

    #[serde(rename = "background_color")]
    pub background_color: Option<String>,
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// Port of `model.Channel` (channel.go:81).
///
/// `type` is a `String`, not a Rust enum, on purpose: Go's `ChannelType` is a defined string
/// type, so `json.Unmarshal` accepts **any** string into it. A closed enum would reject a row
/// written by a newer Go server and turn a forward-compatible read into a hard error. The
/// accepted set is enforced by [`Channel::is_valid`], exactly as in Go.
///
/// `props` and `policy_actions` are both maps, but they serialise differently: `props` has no
/// `omitempty` so a nil map is `null` with the key present, while `policy_actions` has
/// `omitempty` so a nil **or empty** map drops the key entirely.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "type")]
    pub channel_type: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "header")]
    pub header: String,

    #[serde(rename = "purpose")]
    pub purpose: String,

    #[serde(rename = "last_post_at")]
    pub last_post_at: i64,

    #[serde(rename = "total_msg_count")]
    pub total_msg_count: i64,

    #[serde(rename = "extra_update_at")]
    pub extra_update_at: i64,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "scheme_id")]
    pub scheme_id: Option<String>,

    /// `map[string]any` with no `omitempty`: a nil map is `null`, and nil is meaningful —
    /// [`Channel::make_non_nil`] branches on it.
    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "group_constrained")]
    pub group_constrained: Option<bool>,

    #[serde(rename = "autotranslation")]
    pub auto_translation: bool,

    #[serde(rename = "shared")]
    pub shared: Option<bool>,

    #[serde(rename = "total_msg_count_root")]
    pub total_msg_count_root: i64,

    #[serde(rename = "policy_id")]
    pub policy_id: Option<String>,

    #[serde(rename = "last_root_post_at")]
    pub last_root_post_at: i64,

    #[serde(rename = "banner_info")]
    pub banner_info: Option<ChannelBannerInfo>,

    #[serde(rename = "policy_enforced")]
    pub policy_enforced: bool,

    /// Maps each action key declared by the channel's access-control policy to true. Hydrated
    /// lazily by the app layer, so it is absent on most read paths — empty/nil means either no
    /// policy or no hydration, never "no actions". Prefer [`Channel::has_policy_action`] over
    /// indexing it.
    #[serde(
        rename = "policy_actions",
        default,
        skip_serializing_if = "bool_map_is_empty"
    )]
    pub policy_actions: Option<HashMap<String, bool>>,

    #[serde(rename = "policy_is_active")]
    pub policy_is_active: bool,

    #[serde(rename = "default_category_name")]
    pub default_category_name: String,

    #[serde(rename = "managed_category_name")]
    pub managed_category_name: String,

    #[serde(rename = "discoverable")]
    pub discoverable: bool,
}

impl Channel {
    /// Port of `(*Channel).HasPolicyAction` (channel.go:124).
    pub fn has_policy_action(&self, action: &str) -> bool {
        self.policy_actions
            .as_ref()
            .is_some_and(|actions| actions.get(action).copied().unwrap_or(false))
    }

    /// Port of `(*Channel).HasMembershipPolicyAction` (channel.go:136).
    pub fn has_membership_policy_action(&self) -> bool {
        self.has_policy_action(ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP)
    }

    fn error(&self, field: &str, with_id: bool) -> Box<AppError> {
        self.error_inner("Channel.IsValid", field, with_id, None)
    }

    /// `field` is the middle of the error id, dots included: `"is_valid.banner_info.text.empty"`
    /// becomes `model.channel.is_valid.banner_info.text.empty.app_error`.
    fn error_inner(
        &self,
        where_: &str,
        field: &str,
        with_id: bool,
        params: Option<HashMap<String, serde_json::Value>>,
    ) -> Box<AppError> {
        let details = if with_id {
            format!("id={}", self.id)
        } else {
            String::new()
        };
        Box::new(AppError::new(
            where_,
            format!("model.channel.{field}.app_error"),
            params,
            details,
            400,
        ))
    }

    /// Port of `(*Channel).IsValid` (channel.go:310).
    ///
    /// Four results here are surprising enough to be worth stating, all confirmed by the
    /// oracle rather than by reading:
    ///
    /// 1. **`ChannelNameMaxLength` is never enforced.** Only the minimum is, via
    ///    `IsValidChannelIdentifier`. A 65-character channel name is valid.
    /// 2. **An empty `display_name` is valid**, unlike `Team`, which requires one.
    /// 3. **`creator_id` is only length-checked**, not validated as an id — `"nope"` passes.
    /// 4. **The DM/GM name-collision guard applies to `S`, `BO` and `BP` too**, not just
    ///    `O`/`P`. Go's inner `Type != Direct` re-test is dead code, guaranteed by the outer
    ///    condition, and is not reproduced.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(self.error("is_valid.id", false));
        }
        if self.create_at == 0 {
            return Err(self.error("is_valid.create_at", true));
        }
        if self.update_at == 0 {
            return Err(self.error("is_valid.update_at", true));
        }
        if self.display_name.chars().count() > CHANNEL_DISPLAY_NAME_MAX_RUNES {
            return Err(self.error("is_valid.display_name", true));
        }
        if !is_valid_channel_identifier(&self.name) {
            // The error id does not name the field it guards; clients key off it.
            return Err(self.error("is_valid.1_or_more", true));
        }
        if !matches!(
            self.channel_type.as_str(),
            CHANNEL_TYPE_OPEN
                | CHANNEL_TYPE_PRIVATE
                | CHANNEL_TYPE_DIRECT
                | CHANNEL_TYPE_GROUP
                | CHANNEL_TYPE_SPACE
                | CHANNEL_TYPE_OPEN_BOARD
                | CHANNEL_TYPE_PRIVATE_BOARD
        ) {
            return Err(self.error("is_valid.type", true));
        }
        if self.header.chars().count() > CHANNEL_HEADER_MAX_RUNES {
            return Err(self.error("is_valid.header", true));
        }
        if self.purpose.chars().count() > CHANNEL_PURPOSE_MAX_RUNES {
            return Err(self.error("is_valid.purpose", true));
        }
        // Bytes, and not `IsValidId` — a short non-id such as "nope" is accepted.
        if self.creator_id.len() > 26 {
            return Err(self.error("is_valid.creator_id", false));
        }

        if self.channel_type != CHANNEL_TYPE_DIRECT && self.channel_type != CHANNEL_TYPE_GROUP {
            let mut parts = self.name.split("__");
            let looks_like_dm = match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(b), None) => is_valid_id(a) && is_valid_id(b),
                _ => false,
            };
            if matches(&GM_NAME_REGEX, &self.name) || looks_like_dm {
                return Err(self.error("is_valid.name", false));
            }
        }

        if let Some(banner) = &self.banner_info
            && banner.enabled == Some(true)
        {
            if self.channel_type != CHANNEL_TYPE_OPEN && self.channel_type != CHANNEL_TYPE_PRIVATE {
                return Err(self.error("is_valid.banner_info.channel_type", false));
            }

            match banner.text.as_deref() {
                None | Some("") => {
                    return Err(self.error("is_valid.banner_info.text.empty", false));
                }
                // Bytes, not runes: 400 snowmen is 1,200 bytes and is rejected.
                Some(text) if text.len() > CHANNEL_BANNER_INFO_MAX_LENGTH => {
                    let params = HashMap::from([(
                        "maxLength".to_string(),
                        serde_json::Value::from(CHANNEL_BANNER_INFO_MAX_LENGTH),
                    )]);
                    return Err(self.error_inner(
                        "Channel.IsValid",
                        "is_valid.banner_info.text.invalid_length",
                        false,
                        Some(params),
                    ));
                }
                Some(_) => {}
            }

            match banner.background_color.as_deref() {
                None | Some("") => {
                    return Err(self.error("is_valid.banner_info.background_color.empty", false));
                }
                Some(color) if !matches(&CHANNEL_HEX_COLOR_REGEX, color) => {
                    return Err(self.error("is_valid.banner_info.background_color.invalid", false));
                }
                Some(_) => {}
            }
        }

        // Discoverability is a private-channel feature; `S`, `BO` and `BP` are rejected too.
        if self.discoverable && self.channel_type != CHANNEL_TYPE_PRIVATE {
            return Err(self.error("is_valid.discoverable", true));
        }

        if self.is_group_constrained() && !self.supports_group_sync() {
            return Err(self.error("is_valid.group_constrained", true));
        }

        Ok(())
    }

    /// Port of `(*Channel).IsValidBoard` (channel.go:387).
    ///
    /// Note it checks **only** the three board-specific conditions — a board with an empty id
    /// and a zero `create_at` passes. It is a supplement to `is_valid`, not a replacement.
    /// Callers are expected to trim `display_name` first.
    pub fn is_valid_board(&self) -> AppResult {
        if !self.is_board() {
            return Err(self.error_inner(
                "Channel.IsValidBoard",
                "is_valid_board.type",
                false,
                None,
            ));
        }
        if self.team_id.is_empty() {
            return Err(self.error_inner(
                "Channel.IsValidBoard",
                "is_valid_board.team_id",
                false,
                None,
            ));
        }
        if self.display_name.is_empty() {
            return Err(self.error_inner(
                "Channel.IsValidBoard",
                "is_valid_board.display_name",
                false,
                None,
            ));
        }
        Ok(())
    }

    /// Port of `(*Channel).PreSave` (channel.go:405).
    ///
    /// `create_at` is preserved when non-zero (unlike `Team::pre_save`, which overwrites it),
    /// `update_at` is then forced to equal it, and `extra_update_at` is zeroed. Only `name` and
    /// `display_name` are sanitized — `header` and `purpose` are not.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
        self.extra_update_at = 0;
    }

    /// Port of `(*Channel).PreUpdate` (channel.go:418).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
    }

    /// Port of `(*Channel).DeepCopy` (channel.go:302).
    ///
    /// **Rust copies more than Go does.** Go's `cCopy := *o` shallow-copies the struct, so the
    /// copy shares the `props` and `policy_actions` maps and the `banner_info` pointer with the
    /// original; only `scheme_id` is deep-copied. `Clone` here copies everything. Nothing in
    /// the Go tree relies on the sharing, but a port of a call site that mutates the copy's
    /// props and expects the original to change would silently differ. See D-015.
    #[must_use]
    pub fn deep_copy(&self) -> Self {
        self.clone()
    }

    /// Port of `(*Channel).IsGroupOrDirect` (channel.go:424).
    pub fn is_group_or_direct(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_DIRECT || self.channel_type == CHANNEL_TYPE_GROUP
    }

    /// Port of `(*Channel).SupportsGroupSync` (channel.go:429) — whether `group_constrained`
    /// is meaningful for this channel type.
    pub fn supports_group_sync(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_OPEN || self.channel_type == CHANNEL_TYPE_PRIVATE
    }

    /// Port of `(*Channel).IsOpen` (channel.go:433).
    pub fn is_open(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_OPEN
    }

    /// Port of `(*Channel).IsBoard` (channel.go:437).
    pub fn is_board(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_OPEN_BOARD
            || self.channel_type == CHANNEL_TYPE_PRIVATE_BOARD
    }

    /// Port of `(*Channel).IsSpace` (channel.go:441).
    pub fn is_space(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_SPACE
    }

    /// Port of `(*Channel).IsMessageChannel` (channel.go:448). False for boards and spaces.
    pub fn is_message_channel(&self) -> bool {
        matches!(
            self.channel_type.as_str(),
            CHANNEL_TYPE_OPEN | CHANNEL_TYPE_PRIVATE | CHANNEL_TYPE_DIRECT | CHANNEL_TYPE_GROUP
        )
    }

    /// Port of `(*Channel).IsOpenBoard` (channel.go:457).
    pub fn is_open_board(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_OPEN_BOARD
    }

    /// Port of `(*Channel).IsPrivateBoard` (channel.go:461).
    pub fn is_private_board(&self) -> bool {
        self.channel_type == CHANNEL_TYPE_PRIVATE_BOARD
    }

    /// Port of `(*Channel).Patch` (channel.go:465).
    ///
    /// Two asymmetries Go encodes and this reproduces:
    ///
    /// - `display_name` and `default_category_name` are trimmed; `name`, `header` and
    ///   `purpose` are not.
    /// - **`managed_category_name` is declared on the patch but never applied.** Setting it has
    ///   no effect, which is almost certainly an upstream oversight (see D-016). Do not "fix"
    ///   it here — clients would start seeing a field change that the Go server ignores.
    pub fn patch(&mut self, patch: &ChannelPatch) {
        if let Some(display_name) = &patch.display_name {
            self.display_name = display_name.trim().to_string();
        }
        if let Some(name) = &patch.name {
            self.name = name.clone();
        }
        if let Some(header) = &patch.header {
            self.header = header.clone();
        }
        if let Some(purpose) = &patch.purpose {
            self.purpose = purpose.clone();
        }
        if patch.group_constrained.is_some() {
            self.group_constrained = patch.group_constrained;
        }

        if let Some(banner_patch) = &patch.banner_info {
            let banner = self
                .banner_info
                .get_or_insert_with(ChannelBannerInfo::default);
            if banner_patch.enabled.is_some() {
                banner.enabled = banner_patch.enabled;
            }
            if banner_patch.text.is_some() {
                banner.text = banner_patch.text.clone();
            }
            if banner_patch.background_color.is_some() {
                banner.background_color = banner_patch.background_color.clone();
            }
        }

        if let Some(auto_translation) = patch.auto_translation {
            self.auto_translation = auto_translation;
        }
        if let Some(default_category_name) = &patch.default_category_name {
            self.default_category_name = default_category_name.trim().to_string();
        }
        if let Some(discoverable) = patch.discoverable {
            self.discoverable = discoverable;
        }
    }

    /// Port of `(*Channel).MakeNonNil` (channel.go:519).
    pub fn make_non_nil(&mut self) {
        self.props.get_or_insert_with(StringInterface::new);
    }

    /// Port of `(*Channel).AddProp` (channel.go:525).
    pub fn add_prop(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.props
            .get_or_insert_with(StringInterface::new)
            .insert(key.into(), value);
    }

    /// Port of `(*Channel).IsGroupConstrained` (channel.go:531).
    pub fn is_group_constrained(&self) -> bool {
        self.group_constrained == Some(true)
    }

    /// Port of `(*Channel).IsShared` (channel.go:535).
    pub fn is_shared(&self) -> bool {
        self.shared == Some(true)
    }

    /// Port of `(*Channel).GetOtherUserIdForDM` (channel.go:539).
    ///
    /// Returns `""` for a self-DM, for a non-direct channel, and — note — when `user_id` is
    /// not a member at all: Go falls through to `user1` rather than reporting the mismatch.
    pub fn get_other_user_id_for_dm(&self, user_id: &str) -> &str {
        let (user1, user2) = self.get_both_users_for_dm();
        if user2.is_empty() {
            return "";
        }
        if user1 == user_id { user2 } else { user1 }
    }

    /// Port of `(*Channel).GetBothUsersForDM` (channel.go:551).
    ///
    /// A self-DM (`id__id`) returns `(id, "")`, which is how callers detect it. The parts are
    /// not validated as ids — `"a__b"` yields `("a", "b")`.
    pub fn get_both_users_for_dm(&self) -> (&str, &str) {
        if self.channel_type != CHANNEL_TYPE_DIRECT {
            return ("", "");
        }
        let mut parts = self.name.split("__");
        match (parts.next(), parts.next(), parts.next()) {
            (Some(user1), Some(user2), None) => {
                if user1 == user2 {
                    (user1, "")
                } else {
                    (user1, user2)
                }
            }
            _ => ("", ""),
        }
    }

    /// Port of `(*Channel).Sanitize` (channel.go:567).
    ///
    /// Whitelist, not blacklist: only `id`, `team_id`, `type` and `display_name` survive.
    /// `props` — which can carry arbitrary values — is dropped along with everything else.
    #[must_use]
    pub fn sanitize(&self) -> Self {
        Self {
            id: self.id.clone(),
            team_id: self.team_id.clone(),
            channel_type: self.channel_type.clone(),
            display_name: self.display_name.clone(),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Port of `model.GetDMNameFromIds` (channel.go:580).
///
/// Byte-wise ordering, matching Go's string comparison — `"A"` sorts before `"a"`.
pub fn get_dm_name_from_ids(user_id1: &str, user_id2: &str) -> String {
    if user_id1 > user_id2 {
        format!("{user_id2}__{user_id1}")
    } else {
        format!("{user_id1}__{user_id2}")
    }
}

/// Port of `model.GetGroupDisplayNameFromUsers` (channel.go:587).
///
/// Takes an iterator rather than a slice so callers holding `Vec<User>` or `Vec<&User>` can
/// both pass without cloning.
///
/// The truncating branch cuts at `ChannelNameMaxLength` **bytes**, so Go can split a multi-byte
/// character and return invalid UTF-8. [`limit_bytes`] stops at the nearest char boundary
/// instead — the divergence recorded as D-007.
pub fn get_group_display_name_from_users<'a>(
    users: impl IntoIterator<Item = &'a User>,
    truncate: bool,
) -> String {
    let mut usernames: Vec<&str> = users.into_iter().map(|u| u.username.as_str()).collect();
    usernames.sort_unstable();
    let name = usernames.join(", ");

    if truncate && name.len() > CHANNEL_NAME_MAX_LENGTH {
        return limit_bytes(&name, CHANNEL_NAME_MAX_LENGTH).0;
    }
    name
}

const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// Port of `model.GetGroupNameFromUserIds` (channel.go:603).
///
/// The SHA-1 hex digest of the sorted ids concatenated with no separator. This becomes the
/// group channel's `Name` and is persisted, so it must agree with Go bit for bit — the choice
/// of SHA-1 is a compatibility constraint, not a security judgement.
///
/// Go sorts the caller's slice **in place**; this sorts a local vector of borrows, so the
/// caller's ordering is preserved. No Go call site depends on the mutation.
pub fn get_group_name_from_user_ids(user_ids: &[String]) -> String {
    let mut sorted: Vec<&str> = user_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();

    let mut hasher = Sha1::new();
    for id in sorted {
        hasher.update(id.as_bytes());
    }

    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Both indices are masked to 0..=15, so neither can be out of bounds.
        out.push(HEX_DIGITS[usize::from(byte >> 4)]);
        out.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
    }
    out
}

// ---------------------------------------------------------------------------
// Companion wire types
// ---------------------------------------------------------------------------

/// Port of `model.ChannelWithTeamData` (channel.go:170).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelWithTeamData {
    #[serde(flatten)]
    pub channel: Channel,

    #[serde(rename = "team_display_name")]
    pub team_display_name: String,

    #[serde(rename = "team_name")]
    pub team_name: String,

    #[serde(rename = "team_update_at")]
    pub team_update_at: i64,
}

/// Port of `model.ChannelsWithCount` (channel.go:176).
///
/// `channels` has no `omitempty`, so a nil list serialises as `null` rather than `[]`. Landed
/// with `channel_list.go`, which supplies the list type — this closes D-014.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelsWithCount {
    #[serde(rename = "channels")]
    pub channels: Option<ChannelListWithTeamData>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.ChannelPatch` (channel.go:181).
///
/// Every field is a pointer with no `omitempty`, so all ten keys are always present on the
/// wire and `null` means "leave alone".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPatch {
    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "name")]
    pub name: Option<String>,

    #[serde(rename = "header")]
    pub header: Option<String>,

    #[serde(rename = "purpose")]
    pub purpose: Option<String>,

    #[serde(rename = "group_constrained")]
    pub group_constrained: Option<bool>,

    #[serde(rename = "banner_info")]
    pub banner_info: Option<ChannelBannerInfo>,

    #[serde(rename = "autotranslation")]
    pub auto_translation: Option<bool>,

    /// Accepted on the wire and then ignored by [`Channel::patch`] — see D-016.
    #[serde(rename = "managed_category_name")]
    pub managed_category_name: Option<String>,

    #[serde(rename = "default_category_name")]
    pub default_category_name: Option<String>,

    #[serde(rename = "discoverable")]
    pub discoverable: Option<bool>,
}

/// Port of `model.ChannelForExport` (channel.go:205).
///
/// `TeamName` and `SchemeName` carry **no** json tag, so Go marshals them under the Go field
/// names verbatim — capitals included, sitting alongside the inlined snake_case `Channel`
/// fields. Same trap as `TeamForExport`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelForExport {
    #[serde(flatten)]
    pub channel: Channel,

    #[serde(rename = "TeamName")]
    pub team_name: String,

    #[serde(rename = "SchemeName")]
    pub scheme_name: Option<String>,
}

/// Port of `model.DirectChannelForExport` (channel.go:210).
///
/// `Members` has no json tag, so the wire key is the Go field name verbatim, and no
/// `omitempty`, so a nil slice serialises as `null`. Landed with `channel_member.go`, which
/// supplies the element type — the other half of D-014.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DirectChannelForExport {
    #[serde(flatten)]
    pub channel: Channel,

    #[serde(rename = "Members")]
    pub members: Option<Vec<ChannelMemberForExport>>,
}

/// Port of `model.ChannelModeration` (channel.go:220).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModeration {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "roles")]
    pub roles: Option<ChannelModeratedRoles>,
}

/// Port of `model.ChannelModeratedRoles` (channel.go:225).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModeratedRoles {
    #[serde(rename = "guests")]
    pub guests: Option<ChannelModeratedRole>,

    #[serde(rename = "members")]
    pub members: Option<ChannelModeratedRole>,
}

/// Port of `model.ChannelModeratedRole` (channel.go:230).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModeratedRole {
    #[serde(rename = "value")]
    pub value: bool,

    #[serde(rename = "enabled")]
    pub enabled: bool,
}

/// Port of `model.ChannelModerationPatch` (channel.go:235).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModerationPatch {
    #[serde(rename = "name")]
    pub name: Option<String>,

    #[serde(rename = "roles")]
    pub roles: Option<ChannelModeratedRolesPatch>,
}

/// Port of `model.ChannelModeratedRolesPatch` (channel.go:247).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelModeratedRolesPatch {
    #[serde(rename = "guests")]
    pub guests: Option<bool>,

    #[serde(rename = "members")]
    pub members: Option<bool>,
}

/// Port of `model.ChannelMemberCountByGroup` (channel.go:286).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberCountByGroup {
    #[serde(rename = "group_id")]
    pub group_id: String,

    #[serde(rename = "channel_member_count")]
    pub channel_member_count: i64,

    #[serde(rename = "channel_member_timezones_count")]
    pub channel_member_timezones_count: i64,
}

/// Port of `model.GroupMessageConversionRequestBody` (channel.go:617).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMessageConversionRequestBody {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// Query option structs — no json tags in Go, so never on the wire
// ---------------------------------------------------------------------------

/// Port of `model.ChannelSearchOpts` (channel.go:263).
///
/// Go declares no json tags, so this never crosses the wire; it is the argument to the store's
/// channel search. `Page`/`PerPage` are pointers in Go and stay `Option` here — `None` means
/// "not paginated", which is distinct from page zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelSearchOpts {
    pub not_associated_to_group: String,
    pub exclude_default_channels: bool,
    /// When true, channels with a non-zero `delete_at` are included.
    pub include_deleted: bool,
    pub deleted: bool,
    pub exclude_channel_names: Vec<String>,
    pub team_ids: Vec<String>,
    pub group_constrained: bool,
    pub exclude_group_constrained: bool,
    pub policy_id: String,
    pub exclude_policy_constrained: bool,
    pub include_policy_id: bool,
    pub include_search_by_id: bool,
    pub exclude_remote: bool,
    pub public: bool,
    pub private: bool,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    /// With `include_deleted`, only channels deleted after this time are returned.
    pub last_delete_at: i64,
    pub last_update_at: i64,
    pub access_control_policy_enforced: bool,
    pub exclude_access_control_policy_enforced: bool,
    pub parent_access_control_policy_id: String,
}

/// Port of `model.ChannelMembersGetOptions` (channel.go:625).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelMembersGetOptions {
    pub channel_id: String,
    pub offset: i64,
    /// Maximum number of results to return.
    pub limit: i64,
    /// Cursor-based pagination: only members updated after this timestamp.
    pub updated_after: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;

    fn fixture_channel() -> Channel {
        serde_json::from_str(include_str!("../../../fixtures/channel.json")).unwrap()
    }

    /// A channel that passes every `is_valid` branch, so each test can break exactly one thing.
    fn valid_channel() -> Channel {
        Channel {
            id: utils::new_id(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            team_id: utils::new_id(),
            channel_type: CHANNEL_TYPE_OPEN.into(),
            display_name: "Town Square".into(),
            name: DEFAULT_CHANNEL_NAME.into(),
            creator_id: utils::new_id(),
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

    round_trip!(channel_matches_go, Channel, "channel.json");
    round_trip!(
        banner_info_matches_go,
        ChannelBannerInfo,
        "channel_banner_info.json"
    );
    round_trip!(
        channel_with_team_data_matches_go,
        ChannelWithTeamData,
        "channel_with_team_data.json"
    );
    round_trip!(channel_patch_matches_go, ChannelPatch, "channel_patch.json");
    round_trip!(
        channel_for_export_matches_go,
        ChannelForExport,
        "channel_for_export.json"
    );
    round_trip!(
        channels_with_count_matches_go,
        ChannelsWithCount,
        "channels_with_count.json"
    );
    round_trip!(
        direct_channel_for_export_matches_go,
        DirectChannelForExport,
        "direct_channel_for_export.json"
    );
    round_trip!(
        channel_moderation_matches_go,
        ChannelModeration,
        "channel_moderation.json"
    );
    round_trip!(
        channel_moderation_patch_matches_go,
        ChannelModerationPatch,
        "channel_moderation_patch.json"
    );
    round_trip!(
        channel_member_count_by_group_matches_go,
        ChannelMemberCountByGroup,
        "channel_member_count_by_group.json"
    );
    round_trip!(
        group_message_conversion_body_matches_go,
        GroupMessageConversionRequestBody,
        "group_message_conversion_request_body.json"
    );

    #[test]
    fn nil_pointers_serialise_as_null_not_omitted() {
        // None of these carries omitempty in Go, so the key is always present.
        let value = serde_json::to_value(Channel::default()).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "scheme_id",
            "props",
            "group_constrained",
            "shared",
            "policy_id",
            "banner_info",
        ] {
            assert!(object.contains_key(key), "{key} must be present");
            assert!(value[key].is_null(), "{key} must be null");
        }
    }

    #[test]
    fn policy_actions_is_dropped_when_nil_or_empty() {
        // Go's omitempty on a map drops both nil and empty.
        for actions in [None, Some(HashMap::new())] {
            let channel = Channel {
                policy_actions: actions,
                ..Default::default()
            };
            let value = serde_json::to_value(&channel).unwrap();
            assert!(!value.as_object().unwrap().contains_key("policy_actions"));
        }

        let channel = Channel {
            policy_actions: Some(HashMap::from([("membership".into(), true)])),
            ..Default::default()
        };
        let value = serde_json::to_value(&channel).unwrap();
        assert_eq!(value["policy_actions"]["membership"], true);
    }

    #[test]
    fn export_types_keep_gos_untagged_field_names() {
        let value = serde_json::to_value(ChannelForExport::default()).unwrap();
        let object = value.as_object().unwrap();
        assert!(object.contains_key("TeamName"));
        assert!(object.contains_key("SchemeName"));
        assert!(!object.contains_key("team_name"));
    }

    #[test]
    fn fixture_channel_deserialises_with_every_field_set() {
        let channel = fixture_channel();
        assert!(!channel.id.is_empty());
        assert!(channel.props.is_some());
        assert!(channel.banner_info.is_some());
        assert!(channel.policy_actions.is_some());
        assert_eq!(channel.channel_type, CHANNEL_TYPE_PRIVATE);
    }

    #[test]
    fn has_policy_action_tolerates_nil_and_missing_keys() {
        let mut channel = Channel::default();
        assert!(!channel.has_policy_action("membership"));
        assert!(!channel.has_membership_policy_action());

        channel.policy_actions = Some(HashMap::from([("membership".into(), false)]));
        assert!(!channel.has_membership_policy_action());

        channel.policy_actions = Some(HashMap::from([("membership".into(), true)]));
        assert!(channel.has_membership_policy_action());
        assert!(!channel.has_policy_action("other"));
    }

    #[test]
    fn pre_save_generates_id_and_create_at_when_absent() {
        let mut channel = Channel {
            update_at: 999,
            extra_update_at: 777,
            ..Default::default()
        };
        channel.pre_save();

        assert_eq!(channel.id.len(), utils::ID_LENGTH);
        assert!(utils::is_valid_id(&channel.id));
        assert!(channel.create_at > 0);
        assert_eq!(channel.update_at, channel.create_at);
        assert_eq!(channel.extra_update_at, 0);
    }

    #[test]
    fn pre_save_preserves_an_existing_create_at() {
        // Opposite of Team::pre_save, which overwrites create_at unconditionally.
        let mut channel = valid_channel();
        channel.create_at = 1234;
        channel.update_at = 5678;
        channel.pre_save();
        assert_eq!(channel.create_at, 1234);
        assert_eq!(channel.update_at, 1234);
    }

    #[test]
    fn pre_update_bumps_update_at_and_sanitizes_names() {
        let mut channel = valid_channel();
        channel.name = "town\u{202e}square".into();
        channel.display_name = "Town\u{2028}Square".into();
        channel.header = "head\u{202e}er".into();
        channel.update_at = 1;

        channel.pre_update();

        assert_eq!(channel.name, "townsquare");
        assert_eq!(channel.display_name, "TownSquare");
        // header is deliberately not sanitized by Go.
        assert_eq!(channel.header, "head\u{202e}er");
        assert!(channel.update_at > 1);
    }

    #[test]
    fn make_non_nil_and_add_prop() {
        let mut channel = Channel::default();
        assert!(channel.props.is_none());

        channel.make_non_nil();
        assert_eq!(channel.props.as_ref().map(StringInterface::len), Some(0));
        // Now that props is non-nil it must serialise as {} rather than null.
        assert_eq!(
            serde_json::to_value(&channel).unwrap()["props"],
            serde_json::json!({})
        );

        let mut other = Channel::default();
        other.add_prop("k", serde_json::json!(1));
        assert_eq!(other.props.unwrap()["k"], 1);
    }

    #[test]
    fn group_constrained_and_shared_read_the_pointer() {
        let mut channel = Channel::default();
        assert!(!channel.is_group_constrained());
        assert!(!channel.is_shared());

        channel.group_constrained = Some(false);
        channel.shared = Some(false);
        assert!(!channel.is_group_constrained());
        assert!(!channel.is_shared());

        channel.group_constrained = Some(true);
        channel.shared = Some(true);
        assert!(channel.is_group_constrained());
        assert!(channel.is_shared());
    }

    #[test]
    fn deep_copy_does_not_alias_the_original() {
        // Documented divergence: Go's DeepCopy shares props with the original; ours does not.
        let mut original = valid_channel();
        original.add_prop("k", serde_json::json!("v"));

        let mut copy = original.deep_copy();
        copy.add_prop("k", serde_json::json!("changed"));

        assert_eq!(original.props.unwrap()["k"], "v");
        assert_eq!(copy.props.unwrap()["k"], "changed");
    }

    #[test]
    fn is_valid_accepts_the_generated_fixture() {
        // The generator's overrides pin type/name/display_name/banner colour to real values,
        // so the fixture is a valid channel as well as a serialization oracle.
        assert!(fixture_channel().is_valid().is_ok());
    }

    #[test]
    fn is_valid_error_ids_and_status() {
        let mut channel = valid_channel();
        channel.create_at = 0;
        let err = channel.is_valid().unwrap_err();
        assert_eq!(err.id, "model.channel.is_valid.create_at.app_error");
        assert_eq!(err.status_code, 400);
        assert_eq!(err.detailed_error, format!("id={}", channel.id));
    }

    #[test]
    fn is_valid_banner_length_error_carries_max_length_param() {
        let mut channel = valid_channel();
        channel.banner_info = Some(ChannelBannerInfo {
            enabled: Some(true),
            text: Some("a".repeat(CHANNEL_BANNER_INFO_MAX_LENGTH + 1)),
            background_color: Some("#fff".into()),
        });
        let err = channel.is_valid().unwrap_err();
        assert_eq!(
            err.id,
            "model.channel.is_valid.banner_info.text.invalid_length.app_error"
        );
        assert_eq!(err.params.as_ref().unwrap()["maxLength"], 1024);
    }

    #[test]
    fn channel_name_max_length_is_never_enforced() {
        // ChannelNameMaxLength exists but IsValid only checks the minimum.
        let mut channel = valid_channel();
        channel.name = "a".repeat(CHANNEL_NAME_MAX_LENGTH + 1);
        assert!(channel.is_valid().is_ok());
    }

    #[test]
    fn is_valid_channel_identifier_requires_one_character() {
        assert!(!is_valid_channel_identifier(""));
        assert!(is_valid_channel_identifier("a"));
        assert!(!is_valid_channel_identifier("A"));
    }

    #[test]
    fn sanitize_keeps_only_four_fields() {
        let mut channel = fixture_channel();
        channel.header = "secret".into();
        let sanitized = channel.sanitize();

        assert_eq!(sanitized.id, channel.id);
        assert_eq!(sanitized.team_id, channel.team_id);
        assert_eq!(sanitized.channel_type, channel.channel_type);
        assert_eq!(sanitized.display_name, channel.display_name);

        assert!(sanitized.header.is_empty());
        assert!(sanitized.name.is_empty());
        assert!(sanitized.props.is_none());
        assert!(sanitized.banner_info.is_none());
        assert_eq!(sanitized.create_at, 0);
        assert!(!sanitized.discoverable);
    }
}

/// Parity tests driven by `fixtures/behaviour_channel.json` — Go's own answers, not ours.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_channel.json")).unwrap()
    }

    #[test]
    fn hex_color_regex_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_hex_color"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                matches(&CHANNEL_HEX_COLOR_REGEX, input),
                want.as_bool().unwrap(),
                "channelHexColorRegex({input:?})"
            );
        }
    }

    #[test]
    fn gm_name_regex_matches_go() {
        let oracle = oracle();
        let cases = oracle["gm_name_regex"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                matches(&GM_NAME_REGEX, input),
                want.as_bool().unwrap(),
                "gmNameRegex({input:?})"
            );
        }
    }

    #[test]
    fn is_valid_channel_identifier_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid_channel_identifier"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                is_valid_channel_identifier(input),
                want.as_bool().unwrap(),
                "IsValidChannelIdentifier({input:?})"
            );
        }
    }

    /// The core test: every case is deserialized from the JSON Go produced, so a wire-format
    /// drift and a logic drift both fail here.
    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_is_valid"].as_array().unwrap();
        assert!(cases.len() > 50, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let channel: Channel = serde_json::from_value(case["channel"].clone()).unwrap();
            let (got_id, got_detail) = match channel.is_valid() {
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
    fn is_valid_board_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_is_valid_board"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let channel: Channel = serde_json::from_value(case["channel"].clone()).unwrap();
            let got = match channel.is_valid_board() {
                Ok(()) => String::new(),
                Err(err) => err.id.clone(),
            };
            assert_eq!(
                got,
                case["error_id"].as_str().unwrap(),
                "IsValidBoard({name})"
            );
        }
    }

    #[test]
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut channel: Channel = serde_json::from_value(case["before"].clone()).unwrap();
            channel.pre_save();
            assert_eq!(
                serde_json::to_value(&channel).unwrap(),
                case["after"],
                "{name}"
            );
        }
    }

    #[test]
    fn patch_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_patch"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut channel: Channel = serde_json::from_value(case["before"].clone()).unwrap();
            let patch: ChannelPatch = serde_json::from_value(case["patch"].clone()).unwrap();
            channel.patch(&patch);
            assert_eq!(
                serde_json::to_value(&channel).unwrap(),
                case["after"],
                "Patch({name})"
            );
        }
    }

    #[test]
    fn sanitize_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_sanitize"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let channel: Channel = serde_json::from_value(case["before"].clone()).unwrap();
            assert_eq!(
                serde_json::to_value(channel.sanitize()).unwrap(),
                case["after"],
                "Sanitize({})",
                case["name"]
            );
        }
    }

    #[test]
    fn type_predicates_match_go() {
        let oracle = oracle();
        let cases = oracle["channel_type_predicates"].as_object().unwrap();
        assert!(!cases.is_empty());

        for (channel_type, want) in cases {
            let channel = Channel {
                channel_type: channel_type.clone(),
                ..Default::default()
            };
            let got = [
                ("is_group_or_direct", channel.is_group_or_direct()),
                ("supports_group_sync", channel.supports_group_sync()),
                ("is_open", channel.is_open()),
                ("is_board", channel.is_board()),
                ("is_space", channel.is_space()),
                ("is_message_channel", channel.is_message_channel()),
                ("is_open_board", channel.is_open_board()),
                ("is_private_board", channel.is_private_board()),
            ];
            for (predicate, value) in got {
                assert_eq!(
                    value,
                    want[predicate].as_bool().unwrap(),
                    "{predicate}({channel_type:?})"
                );
            }
        }
    }

    #[test]
    fn get_dm_name_from_ids_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_dm_name_from_ids"].as_object().unwrap();
        assert!(!cases.is_empty());

        for (key, want) in cases {
            let (a, b) = key.split_once('|').unwrap();
            assert_eq!(
                get_dm_name_from_ids(a, b),
                want.as_str().unwrap(),
                "GetDMNameFromIds({a:?}, {b:?})"
            );
        }
    }

    #[test]
    fn get_both_users_for_dm_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_both_users_for_dm"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let channel = Channel {
                channel_type: case["type"].as_str().unwrap().to_string(),
                name: case["channel_name"].as_str().unwrap().to_string(),
                ..Default::default()
            };
            let (user1, user2) = channel.get_both_users_for_dm();
            assert_eq!(user1, case["user1"].as_str().unwrap(), "{name} user1");
            assert_eq!(user2, case["user2"].as_str().unwrap(), "{name} user2");

            let first = case["user1"].as_str().unwrap();
            assert_eq!(
                channel.get_other_user_id_for_dm("6bdz674pgq767e4jx75w4pf57a"),
                case["other_for_user1"].as_str().unwrap(),
                "{name} other_for_user1"
            );
            assert_eq!(
                channel.get_other_user_id_for_dm("stranger"),
                case["other_for_stranger"].as_str().unwrap(),
                "{name} other_for_stranger (first={first:?})"
            );
        }
    }

    #[test]
    fn get_group_name_from_user_ids_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_group_name_from_user_ids"].as_object().unwrap();
        assert!(!cases.is_empty());

        for (key, want) in cases {
            // The key is the caller's original order joined with '|'. An empty key is the
            // empty slice; Go's `{""}` case hashes identically and collided into it.
            let ids: Vec<String> = if key.is_empty() {
                Vec::new()
            } else {
                key.split('|').map(str::to_string).collect()
            };
            assert_eq!(
                get_group_name_from_user_ids(&ids),
                want.as_str().unwrap(),
                "GetGroupNameFromUserIds({ids:?})"
            );
        }
    }

    #[test]
    fn get_group_name_from_user_ids_does_not_reorder_the_caller() {
        let ids = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let hash = get_group_name_from_user_ids(&ids);
        assert_eq!(ids, vec!["c", "a", "b"], "input must not be reordered");
        // sha1("abc")
        assert_eq!(hash, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn get_group_display_name_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_group_display_name"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let users: Vec<User> = case["usernames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| User {
                    username: name.as_str().unwrap().to_string(),
                    ..Default::default()
                })
                .collect();
            let truncate = case["truncate"].as_bool().unwrap();
            assert_eq!(
                get_group_display_name_from_users(&users, truncate),
                case["out"].as_str().unwrap(),
                "GetGroupDisplayNameFromUsers(truncate={truncate})"
            );
        }
    }

    #[test]
    fn banner_info_wire_shape_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_banner_info_round_trip"]
            .as_object()
            .unwrap();
        assert!(!cases.is_empty());

        for (name, want) in cases {
            let banner: ChannelBannerInfo = serde_json::from_value(want.clone()).unwrap();
            assert_eq!(&serde_json::to_value(&banner).unwrap(), want, "{name}");
        }
    }
}
