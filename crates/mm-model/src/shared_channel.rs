//! Port of `model/shared_channel.go` — channels synchronised with a remote cluster.
//!
//! # `Home` decides which half of the model applies
//!
//! When `home` is true the channel lives here and the `SharedChannelRemotes` table lists the
//! remotes invited to it. When false it lives elsewhere and `remote_id` points at the connection
//! it came from — which is why `IsValid` only requires `remote_id` in that case.
//!
//! # Every validator here borrows `model.channel.*` error ids
//!
//! Not one error id in this file mentions "shared": they are all `model.channel.is_valid.*`,
//! including on `SharedChannelUser` and `SharedChannelAttachment`, and **four different fields
//! share `…is_valid.id.app_error`** on a single type. Only the details string distinguishes them.
//!
//! # Not ported: the XML codec
//!
//! `SyncMsg` and `SyncResponse` carry `xml:` tags and `SyncMsg` hand-writes `MarshalXML` /
//! `UnmarshalXML` — including `Users>User` and `MentionTransforms>Transform` wrappers that exist
//! because Go's `encoding/xml` cannot marshal a map. This crate has no XML codec (see
//! `xml_helpers.go`, deferred for the same reason); the JSON side, which is what the sync
//! transport actually uses, is ported in full.

use serde::{Deserialize, Serialize};

use crate::channel::{
    CHANNEL_DISPLAY_NAME_MAX_RUNES, CHANNEL_HEADER_MAX_RUNES, CHANNEL_PURPOSE_MAX_RUNES,
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, is_valid_channel_identifier,
};
use crate::post::Post;
use crate::post_acknowledgement::PostAcknowledgement;
use crate::reaction::Reaction;
use crate::remote_cluster::Bitmask;
use crate::serde_helpers::{is_empty_str, is_none_or_empty_vec};
use crate::status::Status;
use crate::user::User;
use crate::utils::{
    AppError, AppResult, StringMap, get_millis, is_valid_id, new_id, sanitize_unicode,
};

/// Port of `model.SharedChannel` (shared_channel.go:35).
///
/// **`ChannelId` is tagged `id`**, and the four `Share*` fields drop the prefix on the wire:
/// `name`, `display_name`, `purpose`, `header`. `Type` is `db:"-"` **and untagged**, so it is
/// joined in from the channel and marshals under its Go field name if this type is ever sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedChannel {
    #[serde(rename = "id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// True when this server owns the channel.
    #[serde(rename = "home")]
    pub home: bool,

    #[serde(rename = "readonly")]
    pub read_only: bool,

    #[serde(rename = "name")]
    pub share_name: String,

    #[serde(rename = "display_name")]
    pub share_display_name: String,

    #[serde(rename = "purpose")]
    pub share_purpose: String,

    #[serde(rename = "header")]
    pub share_header: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    /// Only when **not** `home`.
    #[serde(rename = "remote_id", skip_serializing_if = "is_empty_str")]
    pub remote_id: String,

    /// `db:"-"`, and Go gives it no `json:` tag — so its wire key is `Type`, capitalised, unlike
    /// every other field here.
    #[serde(rename = "Type")]
    pub type_: String,
}

impl SharedChannel {
    /// Port of `(*SharedChannel).IsValid` (shared_channel.go:51).
    ///
    /// A **DM or GM needs no team id** — that is the first branch's real content. The name goes
    /// through `IsValidChannelIdentifier`, so a shared channel's name obeys the same rules as a
    /// local one.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.channel_id);

        if !is_valid_id(&self.channel_id) {
            return Err(err(
                "SharedChannel.IsValid",
                "id",
                format!("ChannelId={}", self.channel_id),
            ));
        }

        if self.type_ != CHANNEL_TYPE_DIRECT
            && self.type_ != CHANNEL_TYPE_GROUP
            && !is_valid_id(&self.team_id)
        {
            return Err(err(
                "SharedChannel.IsValid",
                "id",
                format!("TeamId={}", self.team_id),
            ));
        }

        if self.create_at == 0 {
            return Err(err("SharedChannel.IsValid", "create_at", details()));
        }

        if self.update_at == 0 {
            return Err(err("SharedChannel.IsValid", "update_at", details()));
        }

        if self.share_display_name.chars().count() > CHANNEL_DISPLAY_NAME_MAX_RUNES {
            return Err(err("SharedChannel.IsValid", "display_name", details()));
        }

        // Note the id: an invalid name reports `1_or_more`, the same id `channel.go` uses.
        if !is_valid_channel_identifier(&self.share_name) {
            return Err(err("SharedChannel.IsValid", "1_or_more", details()));
        }

        if self.share_header.chars().count() > CHANNEL_HEADER_MAX_RUNES {
            return Err(err("SharedChannel.IsValid", "header", details()));
        }

        if self.share_purpose.chars().count() > CHANNEL_PURPOSE_MAX_RUNES {
            return Err(err("SharedChannel.IsValid", "purpose", details()));
        }

        if !is_valid_id(&self.creator_id) {
            return Err(err(
                "SharedChannel.IsValid",
                "creator_id",
                format!("CreatorId={}", self.creator_id),
            ));
        }

        if !self.home && !is_valid_id(&self.remote_id) {
            return Err(err(
                "SharedChannel.IsValid",
                "id",
                format!("RemoteId={}", self.remote_id),
            ));
        }

        Ok(())
    }

    /// Port of `(*SharedChannel).PreSave` (shared_channel.go:96).
    ///
    /// **Does not mint an id** — the channel id is the shared channel's key — and sets both
    /// timestamps unconditionally. `purpose` and `header` are *not* sanitised, only the two names.
    pub fn pre_save(&mut self) {
        self.share_name = sanitize_unicode(&self.share_name);
        self.share_display_name = sanitize_unicode(&self.share_display_name);

        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*SharedChannel).PreUpdate` (shared_channel.go:104).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
        self.share_name = sanitize_unicode(&self.share_name);
        self.share_display_name = sanitize_unicode(&self.share_display_name);
    }
}

fn err(where_: &'static str, field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        where_,
        format!("model.channel.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.SharedChannelRemote` (shared_channel.go:112) — one remote invited to a shared
/// channel, plus that remote's sync cursors.
///
/// **`LastPostUpdateID` is tagged `last_post_id`**, not `last_post_update_id`, while its
/// create-side twin is `last_post_create_id`. The pair of cursors exists for the same reason as
/// `ComplianceExportCursor`'s: `UpdateAt` is not unique, so the id breaks ties.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedChannelRemote {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "is_invite_accepted")]
    pub is_invite_accepted: bool,

    #[serde(rename = "is_invite_confirmed")]
    pub is_invite_confirmed: bool,

    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "last_post_update_at")]
    pub last_post_update_at: i64,

    /// Tagged `last_post_id`.
    #[serde(rename = "last_post_id")]
    pub last_post_update_id: String,

    #[serde(rename = "last_post_create_at")]
    pub last_post_create_at: i64,

    #[serde(rename = "last_post_create_id")]
    pub last_post_create_id: String,

    #[serde(rename = "last_members_sync_at")]
    pub last_members_sync_at: i64,
}

impl SharedChannelRemote {
    /// Port of `(*SharedChannelRemote).IsValid` (shared_channel.go:129).
    ///
    /// **`RemoteId` is not validated** despite being the point of the row.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err(
                "SharedChannelRemote.IsValid",
                "id",
                format!("Id={}", self.id),
            ));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err(
                "SharedChannelRemote.IsValid",
                "id",
                format!("ChannelId={}", self.channel_id),
            ));
        }

        if self.create_at == 0 {
            return Err(err(
                "SharedChannelRemote.IsValid",
                "create_at",
                format!("id={}", self.channel_id),
            ));
        }

        if self.update_at == 0 {
            return Err(err(
                "SharedChannelRemote.IsValid",
                "update_at",
                format!("id={}", self.channel_id),
            ));
        }

        if !is_valid_id(&self.creator_id) {
            return Err(err(
                "SharedChannelRemote.IsValid",
                "creator_id",
                format!("id={}", self.creator_id),
            ));
        }

        Ok(())
    }

    /// Port of `(*SharedChannelRemote).PreSave` (shared_channel.go:152).
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*SharedChannelRemote).PreUpdate` (shared_channel.go:160).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }
}

/// Port of `model.SharedChannelRemoteStatus` (shared_channel.go:164) — the System Console's view.
///
/// **`Token` is on the wire.**
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedChannelRemoteStatus {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "site_url")]
    pub site_url: String,

    #[serde(rename = "last_ping_at")]
    pub last_ping_at: i64,

    #[serde(rename = "last_sync_at")]
    pub last_sync_at: i64,

    #[serde(rename = "readonly")]
    pub read_only: bool,

    #[serde(rename = "is_invite_accepted")]
    pub is_invite_accepted: bool,

    #[serde(rename = "token")]
    pub token: String,
}

/// Port of `model.SharedChannelUser` (shared_channel.go:178) — one remote's sync cursor for one
/// user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedChannelUser {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "last_sync_at")]
    pub last_sync_at: i64,
}

impl SharedChannelUser {
    /// Port of `(*SharedChannelUser).PreSave` (shared_channel.go:187).
    ///
    /// **Overwrites the id and the timestamp unconditionally** — unlike its sibling on
    /// `SharedChannelRemote`, which fills them only when empty.
    pub fn pre_save(&mut self) {
        self.id = new_id();
        self.create_at = get_millis();
    }

    /// Port of `(*SharedChannelUser).IsValid` (shared_channel.go:192) — **four** fields sharing
    /// one error id.
    pub fn is_valid(&self) -> AppResult {
        for (value, label) in [
            (&self.id, "Id"),
            (&self.user_id, "UserId"),
            (&self.channel_id, "ChannelId"),
            (&self.remote_id, "RemoteId"),
        ] {
            if !is_valid_id(value) {
                return Err(err(
                    "SharedChannelUser.IsValid",
                    "id",
                    format!("{label}={value}"),
                ));
            }
        }

        if self.create_at == 0 {
            return Err(err("SharedChannelUser.IsValid", "create_at", String::new()));
        }

        Ok(())
    }
}

/// Port of `model.GetUsersForSyncFilter` (shared_channel.go:215). No tags. `Limit` is a `uint64`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetUsersForSyncFilter {
    pub check_profile_image: bool,
    pub channel_id: String,
    pub limit: u64,
}

/// Port of `model.SharedChannelAttachment` (shared_channel.go:223).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedChannelAttachment {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "file_id")]
    pub file_id: String,

    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "last_sync_at")]
    pub last_sync_at: i64,
}

impl SharedChannelAttachment {
    /// Port of `(*SharedChannelAttachment).PreSave` (shared_channel.go:231).
    ///
    /// The branch is the interesting part: on **first** save `last_sync_at` is set to
    /// `create_at`, and on every later one it is set to *now* while `create_at` is left alone —
    /// so this one method serves as both save and touch.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        if self.create_at == 0 {
            self.create_at = get_millis();
            self.last_sync_at = self.create_at;
        } else {
            self.last_sync_at = get_millis();
        }
    }

    /// Port of `(*SharedChannelAttachment).IsValid` (shared_channel.go:243).
    pub fn is_valid(&self) -> AppResult {
        for (value, label) in [
            (&self.id, "Id"),
            (&self.file_id, "FileId"),
            (&self.remote_id, "RemoteId"),
        ] {
            if !is_valid_id(value) {
                return Err(err(
                    "SharedChannelAttachment.IsValid",
                    "id",
                    format!("{label}={value}"),
                ));
            }
        }

        if self.create_at == 0 {
            return Err(err(
                "SharedChannelAttachment.IsValid",
                "create_at",
                String::new(),
            ));
        }

        Ok(())
    }
}

/// Port of `model.SharedChannelFilterOpts` (shared_channel.go:262). No tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedChannelFilterOpts {
    pub team_id: String,
    pub creator_id: String,
    pub member_id: String,
    pub exclude_home: bool,
    pub exclude_remote: bool,
}

/// Port of `model.SharedChannelRemoteFilterOpts` (shared_channel.go:270). No tags.
///
/// `IncludeUnconfirmed` and `ExcludeConfirmed` are **not** opposites: the first widens the set,
/// the second narrows it, and both may be set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedChannelRemoteFilterOpts {
    pub channel_id: String,
    pub remote_id: String,
    pub include_unconfirmed: bool,
    pub exclude_confirmed: bool,
    pub exclude_home: bool,
    pub exclude_remote: bool,
    pub include_deleted: bool,
}

/// Port of `model.MembershipChangeMsg` (shared_channel.go:281).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MembershipChangeMsg {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// True for a join, false for a leave.
    #[serde(rename = "is_add")]
    pub is_add: bool,

    #[serde(rename = "remote_id")]
    pub remote_id: String,

    /// Epoch milliseconds.
    #[serde(rename = "change_time")]
    pub change_time: i64,
}

/// Port of `model.SyncMsg` (shared_channel.go:291) — a batch of changes, sent as the payload of a
/// `RemoteClusterMsg`.
///
/// Every collection carries `omitempty`, so a sync carrying only posts is three keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncMsg {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Keyed by user id.
    #[serde(rename = "users", skip_serializing_if = "is_none_or_empty_user_map")]
    pub users: Option<std::collections::BTreeMap<String, User>>,

    #[serde(rename = "posts", skip_serializing_if = "is_none_or_empty_vec")]
    pub posts: Option<Vec<Post>>,

    #[serde(rename = "reactions", skip_serializing_if = "is_none_or_empty_vec")]
    pub reactions: Option<Vec<Reaction>>,

    #[serde(rename = "statuses", skip_serializing_if = "is_none_or_empty_vec")]
    pub statuses: Option<Vec<Status>>,

    #[serde(
        rename = "membership_changes",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub membership_changes: Option<Vec<MembershipChangeMsg>>,

    #[serde(
        rename = "acknowledgements",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub acknowledgements: Option<Vec<PostAcknowledgement>>,

    /// Remote username → local username, for rewriting `@mentions` across clusters.
    #[serde(
        rename = "mention_transforms",
        skip_serializing_if = "is_none_or_empty_string_map"
    )]
    pub mention_transforms: Option<StringMap>,
}

fn is_none_or_empty_user_map(m: &Option<std::collections::BTreeMap<String, User>>) -> bool {
    match m {
        None => true,
        Some(inner) => inner.is_empty(),
    }
}

fn is_none_or_empty_string_map(m: &Option<StringMap>) -> bool {
    crate::serde_helpers::is_none_or_empty_string_map(m)
}

impl SyncMsg {
    /// Port of `model.NewSyncMsg` (shared_channel.go:303) — an id and a channel, nothing else.
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            channel_id: channel_id.into(),
            ..Default::default()
        }
    }

    /// Port of `(*SyncMsg).ToJSON` (shared_channel.go:310).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        crate::utils::go_json_marshal(self)
    }
}

/// Port of `(*SyncMsg).String` (shared_channel.go:318) — the JSON, or **`""` on a marshal
/// error**, which is why it cannot fail.
impl std::fmt::Display for SyncMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_json() {
            Ok(json) => f.write_str(&json),
            Err(_) => Ok(()),
        }
    }
}

/// Port of `model.SyncResponse` (shared_channel.go:526) — what the receiving cluster reports back.
///
/// **Only `membership_errors` carries `omitempty`**; the other six lists are always written, as
/// `null` when nil. The three `*LastUpdateAt` cursors are how the sender advances.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncResponse {
    #[serde(rename = "users_last_update_at")]
    pub users_last_update_at: i64,

    #[serde(rename = "user_errors")]
    pub user_errors: Option<Vec<String>>,

    #[serde(rename = "users_syncd")]
    pub users_syncd: Option<Vec<String>>,

    #[serde(rename = "posts_last_update_at")]
    pub posts_last_update_at: i64,

    #[serde(rename = "post_errors")]
    pub post_errors: Option<Vec<String>>,

    #[serde(rename = "reactions_last_update_at")]
    pub reactions_last_update_at: i64,

    #[serde(rename = "reaction_errors")]
    pub reaction_errors: Option<Vec<String>>,

    #[serde(rename = "acknowledgements_last_update_at")]
    pub acknowledgements_last_update_at: i64,

    #[serde(rename = "acknowledgement_errors")]
    pub acknowledgement_errors: Option<Vec<String>>,

    /// User ids whose status sync failed — **ids, not messages**, unlike the other error lists.
    #[serde(rename = "status_errors")]
    pub status_errors: Option<Vec<String>>,

    #[serde(
        rename = "membership_errors",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub membership_errors: Option<Vec<String>>,
}

/// Port of `model.RegisterPluginOpts` (shared_channel.go:547). No tags — a plugin API argument.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisterPluginOpts {
    pub displayname: String,
    pub plugin_id: String,
    /// The user or bot registering.
    pub creator_id: String,
    pub auto_share_dms: bool,
    pub auto_invited: bool,

    /// The remote endpoint, stored directly as `RemoteCluster.SiteURL` and **unique across all
    /// remotes**. Empty defaults to `plugin_<PluginID>` for single-remote plugins; re-registering
    /// with the same site URL is idempotent and preserves the sync cursors. A plugin registers
    /// several remotes by calling with different site URLs — `nats://nats:4222`,
    /// `https://matrix.org`.
    pub site_url: String,
}

impl RegisterPluginOpts {
    /// Port of `(RegisterPluginOpts).GetOptionFlags` (shared_channel.go:569).
    pub fn get_option_flags(&self) -> Bitmask {
        let mut flags = Bitmask::default();
        if self.auto_share_dms {
            flags.set_bit(Bitmask::AUTO_SHARE_DMS);
        }
        if self.auto_invited {
            flags.set_bit(Bitmask::AUTO_INVITED);
        }
        flags
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
    fn shared_channel_round_trips_the_fixture() {
        assert_fixture_round_trips!(SharedChannel, "shared_channel");
    }
    #[test]
    fn shared_channel_remote_round_trips_the_fixture() {
        assert_fixture_round_trips!(SharedChannelRemote, "shared_channel_remote");
    }
    #[test]
    fn shared_channel_remote_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(SharedChannelRemoteStatus, "shared_channel_remote_status");
    }
    #[test]
    fn shared_channel_user_round_trips_the_fixture() {
        assert_fixture_round_trips!(SharedChannelUser, "shared_channel_user");
    }
    #[test]
    fn shared_channel_attachment_round_trips_the_fixture() {
        assert_fixture_round_trips!(SharedChannelAttachment, "shared_channel_attachment");
    }
    #[test]
    fn membership_change_msg_round_trips_the_fixture() {
        assert_fixture_round_trips!(MembershipChangeMsg, "membership_change_msg");
    }
    #[test]
    fn sync_msg_round_trips_the_fixture() {
        assert_fixture_round_trips!(SyncMsg, "sync_msg");
    }
    #[test]
    fn sync_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(SyncResponse, "sync_response");
    }
}
