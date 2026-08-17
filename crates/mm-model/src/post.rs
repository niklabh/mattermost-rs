//! Port of `server/public/model/post.go` — **chunks 1 and 2 of several**.
//!
//! post.go is 1,640 lines and is being translated across sessions. This module holds the `Post`
//! wire type, the constants, `IsValid`, the pre-hooks, the props accessors and the predicate
//! family — everything that is self-contained — plus the attachment readers that landed with
//! `message_attachment.go` (`Attachments`, `AttachmentsEqual`) and the two tree readers that
//! landed with `post_interactive_blocks.go` (`AllStrings`, `InteractiveBlocksImageURLs`), whose
//! walkers live in [`crate::post_interactive_blocks`], plus `ToJSON`/`EncodeJSON`, unblocked by
//! `Post::strip_action_integrations` in [`crate::integration_action`].
//!
//! `ChannelMentions`, `ChannelMentionsAll` and `ChannelMentionsAllWithOptions` are methods on
//! `Post` but live in [`crate::channel_mentions`] alongside the free functions they wrap — the
//! whole surface is one regex whose Go and Rust spellings disagree, and splitting it would put
//! the trap and its explanation in different files.
//!
//! Not yet ported, each waiting on a dependency rather than on effort:
//!
//! | Go | waits on |
//! |---|---|
//! | `propsIsValid`, `ValidateProps`, `nonEmptyInteractivePayloadPropKeys` | the markdown parser ([D-044]) and `ValidateMmBlocksActions` ([D-042]) |
//! | `RewriteImageURLs`, `WithRewrittenImageURLs` | `shared/markdown` |
//! | `GetPreviewPost`, `ForPlugin` | `permalink.go` |
//! | `Auditable`, `LogClone` | the audit layer ([D-028]) |
//! | the `Rewrite*` and `ReportPost*` families | their own chunk |
//!
//! `Post.Props` is guarded by an unexported `sync.RWMutex` in Go because `Post` is shared
//! between goroutines by pointer. Rust's `&mut self` is the same guarantee enforced by the
//! compiler, so the mutex has no counterpart here and the `GetProps`/`SetProps` accessors exist
//! only because Go's call sites use them.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::message_attachment::MessageAttachment;
use crate::post_interactive_blocks::{
    append_human_readable_interactive_strings, interactive_blocks_image_urls,
};
use crate::post_metadata::PostMetadata;
use crate::user::User;
use crate::utils::{
    AppError, AppResult, StringArray, StringInterface, array_to_json, etag, get_millis,
    go_format_v, go_json_marshal, go_to_lower, is_valid_id, new_id, remove_duplicate_strings,
    string_interface_to_json,
};

/// `PostPriority`'s Go home is post.go, but `PostMetadata` embeds it and `Post` embeds
/// `PostMetadata` — the two files are mutually dependent. The definition lives in
/// `post_metadata.rs` and is re-exported here so call-site translation stays mechanical.
pub use crate::post_metadata::PostPriority;

// --- constants ---------------------------------------------------------------------------

pub const POST_SYSTEM_MESSAGE_PREFIX: &str = "system_";
pub const POST_TYPE_DEFAULT: &str = "";
pub const POST_TYPE_MESSAGE_ATTACHMENT: &str = "slack_attachment";
pub const POST_TYPE_SYSTEM_GENERIC: &str = "system_generic";
/// Deprecated in Go: use [`POST_TYPE_JOIN_CHANNEL`] or [`POST_TYPE_LEAVE_CHANNEL`].
pub const POST_TYPE_JOIN_LEAVE: &str = "system_join_leave";
pub const POST_TYPE_JOIN_CHANNEL: &str = "system_join_channel";
pub const POST_TYPE_GUEST_JOIN_CHANNEL: &str = "system_guest_join_channel";
pub const POST_TYPE_LEAVE_CHANNEL: &str = "system_leave_channel";
pub const POST_TYPE_JOIN_TEAM: &str = "system_join_team";
pub const POST_TYPE_LEAVE_TEAM: &str = "system_leave_team";
pub const POST_TYPE_AUTO_RESPONDER: &str = "system_auto_responder";
pub const POST_TYPE_AUTOTRANSLATION_CHANGE: &str = "system_autotranslation";
/// Deprecated in Go: use [`POST_TYPE_ADD_TO_CHANNEL`] or [`POST_TYPE_REMOVE_FROM_CHANNEL`].
pub const POST_TYPE_ADD_REMOVE: &str = "system_add_remove";
pub const POST_TYPE_ADD_TO_CHANNEL: &str = "system_add_to_channel";
pub const POST_TYPE_ADD_GUEST_TO_CHANNEL: &str = "system_add_guest_to_chan";
pub const POST_TYPE_REMOVE_FROM_CHANNEL: &str = "system_remove_from_channel";
pub const POST_TYPE_MOVE_CHANNEL: &str = "system_move_channel";
pub const POST_TYPE_ADD_TO_TEAM: &str = "system_add_to_team";
pub const POST_TYPE_REMOVE_FROM_TEAM: &str = "system_remove_from_team";
/// 24 characters — the values stay inside the `Posts.Type varchar(26)` limit, which is why Go
/// abbreviates "access_control" to "abac".
pub const POST_TYPE_ACCESS_CONTROL_TEAM_REMOVAL: &str = "system_team_abac_removal";
/// 25 characters. See [`POST_TYPE_ACCESS_CONTROL_TEAM_REMOVAL`].
pub const POST_TYPE_ACCESS_CONTROL_TEAM_ADDITION: &str = "system_team_abac_addition";
pub const POST_TYPE_HEADER_CHANGE: &str = "system_header_change";
pub const POST_TYPE_DISPLAYNAME_CHANGE: &str = "system_displayname_change";
pub const POST_TYPE_CONVERT_CHANNEL: &str = "system_convert_channel";
pub const POST_TYPE_PURPOSE_CHANGE: &str = "system_purpose_change";
pub const POST_TYPE_CHANNEL_DELETED: &str = "system_channel_deleted";
pub const POST_TYPE_CHANNEL_RESTORED: &str = "system_channel_restored";
/// Declared by Go and **not accepted by `IsValid`** — see the module notes.
pub const POST_TYPE_EPHEMERAL: &str = "system_ephemeral";
pub const POST_TYPE_CHANGE_CHANNEL_PRIVACY: &str = "system_change_chan_privacy";
pub const POST_TYPE_WRANGLER: &str = "system_wrangler";
pub const POST_TYPE_GM_CONVERTED_TO_CHANNEL: &str = "system_gm_to_channel";
pub const POST_TYPE_ADD_BOT_TEAMS_CHANNELS: &str = "add_bot_teams_channels";
pub const POST_TYPE_ME: &str = "me";
pub const POST_CUSTOM_TYPE_PREFIX: &str = "custom_";
pub const POST_TYPE_REMINDER: &str = "reminder";
pub const POST_TYPE_BURN_ON_READ: &str = "burn_on_read";
pub const POST_TYPE_CARD: &str = "card";
/// A system post for share/unshare events; the client translates it from props.
pub const POST_TYPE_SHARED_CHANNEL_STATE: &str = "system_shared_chan_state";

pub const POST_FILEIDS_MAX_RUNES: usize = 300;
pub const POST_FILENAMES_MAX_RUNES: usize = 4000;
pub const POST_HASHTAGS_MAX_RUNES: usize = 1000;
pub const POST_MESSAGE_MAX_RUNES_V1: usize = 4000;
pub const POST_MESSAGE_MAX_BYTES_V2: usize = 65535;
/// Go: `PostMessageMaxBytesV2 / 4`, assuming a worst-case UTF-8 representation.
pub const POST_MESSAGE_MAX_RUNES_V2: usize = POST_MESSAGE_MAX_BYTES_V2 / 4;

pub const MAX_REPORTING_PER_PAGE: i64 = 1000;
pub const REPORTING_TIME_FIELD_CREATE_AT: &str = "create_at";
pub const REPORTING_TIME_FIELD_UPDATE_AT: &str = "update_at";
pub const REPORTING_SORT_DIRECTION_ASC: &str = "asc";
pub const REPORTING_SORT_DIRECTION_DESC: &str = "desc";
pub const POST_PROPS_MAX_RUNES: usize = 800_000;
/// Go leaves 40,000 runes of headroom for system and pre-save modifications.
pub const POST_PROPS_MAX_USER_RUNES: usize = POST_PROPS_MAX_RUNES - 40_000;

pub const PROPS_ADD_CHANNEL_MEMBER: &str = "add_channel_member";

pub const POST_PROPS_ADDED_USER_ID: &str = "addedUserId";
pub const POST_PROPS_DELETE_BY: &str = "deleteBy";
pub const POST_PROPS_OVERRIDE_ICON_URL: &str = "override_icon_url";
pub const POST_PROPS_OVERRIDE_ICON_EMOJI: &str = "override_icon_emoji";
pub const POST_PROPS_OVERRIDE_USERNAME: &str = "override_username";
pub const POST_PROPS_FROM_WEBHOOK: &str = "from_webhook";
pub const POST_PROPS_FROM_BOT: &str = "from_bot";
pub const POST_PROPS_FROM_OAUTH_APP: &str = "from_oauth_app";
pub const POST_PROPS_WEBHOOK_DISPLAY_NAME: &str = "webhook_display_name";
pub const POST_PROPS_FROM_PLUGIN: &str = "from_plugin";
pub const POST_PROPS_MENTION_HIGHLIGHT_DISABLED: &str = "mentionHighlightDisabled";
pub const POST_PROPS_GROUP_HIGHLIGHT_DISABLED: &str = "disable_group_highlight";
pub const POST_PROPS_PREVIEWED_POST: &str = "previewed_post";
pub const POST_PROPS_FORCE_NOTIFICATION: &str = "force_notification";
pub const POST_PROPS_SILENT_NOTIFICATION: &str = "silent_notification";
pub const POST_PROPS_CHANNEL_MENTIONS: &str = "channel_mentions";
pub const POST_PROPS_CURRENT_TEAM_ID: &str = "current_team_id";
pub const POST_PROPS_UNSAFE_LINKS: &str = "unsafe_links";
pub const POST_PROPS_AI_GENERATED_BY_USER_ID: &str = "ai_generated_by";
pub const POST_PROPS_AI_GENERATED_BY_USERNAME: &str = "ai_generated_by_username";
pub const POST_PROPS_EXPIRE_AT: &str = "expire_at";
pub const POST_PROPS_READ_DURATION_SECONDS: &str = "read_duration";
pub const POST_PROPS_SHARED_CHANNEL_STATE: &str = "shared_channel_state";
pub const POST_PROPS_SHARED_CHANNEL_WORKSPACE_NAME: &str = "workspace_name";

pub const POST_PROPS_ATTACHMENTS: &str = "attachments";
pub const POST_PROPS_MM_BLOCKS: &str = "mm_blocks";
pub const POST_PROPS_BLOCK_KIT_BLOCKS: &str = "blocks";
pub const POST_PROPS_ADAPTIVE_CARDS: &str = "cards";
pub const POST_PROPS_MM_BLOCKS_ACTIONS: &str = "mm_blocks_actions";

pub const POST_PRIORITY_URGENT: &str = "urgent";

/// 7 days, in seconds.
pub const DEFAULT_EXPIRY_SECONDS: i64 = 60 * 60 * 24 * 7;
/// 10 minutes, in seconds.
pub const DEFAULT_READ_DURATION_SECONDS: i64 = 10 * 60;

/// Go's `PostContextKeyIsScheduledPost`, a `PostContextKey` (a defined string type used as a
/// `context.Context` key). Rust has no equivalent carrier; the value is pinned so a future
/// request-extension port can reuse it.
pub const POST_CONTEXT_KEY_IS_SCHEDULED_POST: &str = "isScheduledPost";

/// Values for [`POST_PROPS_SHARED_CHANNEL_STATE`] on [`POST_TYPE_SHARED_CHANNEL_STATE`] posts.
pub const SHARED_CHANNEL_STATE_POST_VALUE_SHARED: &str = "shared";
pub const SHARED_CHANNEL_STATE_POST_VALUE_UNSHARED: &str = "unshared";

/// The post types `IsValid` accepts outright. Anything else must carry the
/// [`POST_CUSTOM_TYPE_PREFIX`].
///
/// [`POST_TYPE_EPHEMERAL`] is deliberately absent — Go's switch does not list it.
const VALID_POST_TYPES: [&str; 35] = [
    POST_TYPE_DEFAULT,
    POST_TYPE_SYSTEM_GENERIC,
    POST_TYPE_JOIN_LEAVE,
    POST_TYPE_AUTO_RESPONDER,
    POST_TYPE_ADD_REMOVE,
    POST_TYPE_JOIN_CHANNEL,
    POST_TYPE_GUEST_JOIN_CHANNEL,
    POST_TYPE_LEAVE_CHANNEL,
    POST_TYPE_JOIN_TEAM,
    POST_TYPE_LEAVE_TEAM,
    POST_TYPE_ADD_TO_CHANNEL,
    POST_TYPE_ADD_GUEST_TO_CHANNEL,
    POST_TYPE_REMOVE_FROM_CHANNEL,
    POST_TYPE_MOVE_CHANNEL,
    POST_TYPE_ADD_TO_TEAM,
    POST_TYPE_REMOVE_FROM_TEAM,
    POST_TYPE_ACCESS_CONTROL_TEAM_REMOVAL,
    POST_TYPE_ACCESS_CONTROL_TEAM_ADDITION,
    POST_TYPE_MESSAGE_ATTACHMENT,
    POST_TYPE_HEADER_CHANGE,
    POST_TYPE_PURPOSE_CHANGE,
    POST_TYPE_DISPLAYNAME_CHANGE,
    POST_TYPE_CONVERT_CHANNEL,
    POST_TYPE_CHANNEL_DELETED,
    POST_TYPE_CHANNEL_RESTORED,
    POST_TYPE_CHANGE_CHANNEL_PRIVACY,
    POST_TYPE_ADD_BOT_TEAMS_CHANNELS,
    POST_TYPE_REMINDER,
    POST_TYPE_ME,
    POST_TYPE_WRANGLER,
    POST_TYPE_GM_CONVERTED_TO_CHANNEL,
    POST_TYPE_AUTOTRANSLATION_CHANGE,
    POST_TYPE_BURN_ON_READ,
    POST_TYPE_CARD,
    POST_TYPE_SHARED_CHANNEL_STATE,
];

/// Go's `postIdentityPropsPreservedOnUpdate` (post.go:643) — server-controlled markers
/// re-applied after `SanitizeProps` during an update so an edit cannot strip integration
/// identity. [`POST_PROPS_FORCE_NOTIFICATION`] is deliberately **not** in this set.
const POST_IDENTITY_PROPS_PRESERVED_ON_UPDATE: [&str; 5] = [
    POST_PROPS_SILENT_NOTIFICATION,
    POST_PROPS_FROM_BOT,
    POST_PROPS_FROM_WEBHOOK,
    POST_PROPS_FROM_OAUTH_APP,
    POST_PROPS_FROM_PLUGIN,
];

/// Go's `reservedProps` (post.go:686). **Order is wire surface**: the returned slice follows
/// this declaration order, not the props map's.
const INTEGRATIONS_RESERVED_PROPS: [&str; 9] = [
    POST_PROPS_FROM_WEBHOOK,
    POST_PROPS_FROM_PLUGIN,
    POST_PROPS_SILENT_NOTIFICATION,
    POST_PROPS_FORCE_NOTIFICATION,
    POST_PROPS_OVERRIDE_USERNAME,
    POST_PROPS_WEBHOOK_DISPLAY_NAME,
    POST_PROPS_OVERRIDE_ICON_URL,
    POST_PROPS_OVERRIDE_ICON_EMOJI,
    POST_PROPS_MM_BLOCKS_ACTIONS,
];

fn is_false(b: &bool) -> bool {
    !*b
}

/// Every `Post::is_valid` failure shares the `Post.IsValid` where-clause, the
/// `model.post.is_valid.<field>.app_error` id shape and a 400.
fn error(
    field: &str,
    params: Option<HashMap<String, serde_json::Value>>,
    details: String,
) -> Box<AppError> {
    Box::new(AppError::new(
        "Post.IsValid",
        format!("model.post.is_valid.{field}.app_error"),
        params,
        details,
        400,
    ))
}

// --- Post ----------------------------------------------------------------------------------

/// Port of `model.Post` (post.go:137).
///
/// Three field shapes decide the wire format:
///
/// - `props`, `file_ids` and `participants` have **no** `omitempty`, so a nil map or slice is
///   `null` and the key is always present. They are `Option<_>` for that reason.
/// - `message_source` and `has_reactions` are non-pointers *with* `omitempty`, so they are a
///   plain `String`/`bool` plus a skip predicate — not `Option`.
/// - `remote_id` and `is_following` are pointers with `omitempty`, where Go's `omitempty` tests
///   only nil-ness. `Some("")` therefore serialises as `""` and `Some(false)` as `false`.
///
/// The container carries `#[serde(default)]` because Go's `encoding/json` leaves an absent field
/// at its zero value, and inbound posts are **always** partial — a client creating a post sends
/// `channel_id` and `message` and nothing else. Without it serde rejects a document the Go
/// server accepts. See [D-043] for the other types in the crate that still need this.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Post {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "edit_at")]
    pub edit_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "is_pinned")]
    pub is_pinned: bool,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "root_id")]
    pub root_id: String,

    #[serde(rename = "original_id")]
    pub original_id: String,

    #[serde(rename = "message")]
    pub message: String,

    /// The message as the user submitted it, when `message` has been rewritten for presentation
    /// (an image proxy, say). Clients populate edit boxes from this when present.
    #[serde(
        rename = "message_source",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub message_source: String,

    #[serde(rename = "type")]
    pub post_type: String,

    /// Go marks the field `Deprecated: use GetProps()` because of the mutex; see the module
    /// docs for why there is no mutex here.
    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "hashtags")]
    pub hashtags: String,

    /// `json:"-"`, and deprecated in Go — yet still measured by [`Self::is_valid`].
    #[serde(skip)]
    pub filenames: StringArray,

    #[serde(rename = "file_ids")]
    pub file_ids: Option<StringArray>,

    #[serde(rename = "pending_post_id")]
    pub pending_post_id: String,

    #[serde(rename = "has_reactions", default, skip_serializing_if = "is_false")]
    pub has_reactions: bool,

    #[serde(rename = "remote_id", default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,

    #[serde(rename = "reply_count")]
    pub reply_count: i64,

    #[serde(rename = "last_reply_at")]
    pub last_reply_at: i64,

    #[serde(rename = "participants")]
    pub participants: Option<Vec<User>>,

    /// For root posts in collapsed-thread mode: whether the current user follows the thread.
    #[serde(
        rename = "is_following",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub is_following: Option<bool>,

    #[serde(rename = "metadata", default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PostMetadata>,
}

/// The failure modes of [`Post::encode_json`]. Go returns a bare `error` covering both.
#[derive(Debug, thiserror::Error)]
pub enum EncodeJsonError {
    #[error("serialize post: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("write post: {0}")]
    Write(#[from] std::io::Error),
}

impl Post {
    /// Port of `(*Post).Etag` (post.go:496).
    pub fn etag(&self) -> String {
        etag(&[&self.id, &self.update_at])
    }

    /// Port of `(*Post).ToJSON` (post.go:402).
    ///
    /// Strips the private action integrations from a **copy**, so the receiver is untouched —
    /// the opposite of [`Self::encode_json`], which strips in place. Getting the two the wrong
    /// way round would either leak an integration or destroy one, so the asymmetry is pinned
    /// separately for each.
    ///
    /// Marshalled through [`go_json_marshal`] rather than `serde_json::to_string`: Go's
    /// `encoding/json` escapes `<`, `>`, `&`, U+2028 and U+2029, and a post's props are exactly
    /// where those characters turn up. See [D-027].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        // Go clones with `ShallowCopy`, which aliases the props map — but `StripActionIntegrations`
        // only ever reaches it through `AddProp`/`DelProp`, both of which swap in a fresh map. So
        // the original is genuinely untouched in Go too, and our owned clone matches. See [D-036].
        let mut copy = self.clone();
        copy.strip_action_integrations();
        go_json_marshal(&copy)
    }

    /// Port of `(*Post).EncodeJSON` (post.go:409).
    ///
    /// **Mutates the receiver**: unlike [`Self::to_json`] it strips the action integrations from
    /// `self`, not from a copy, so the post is permanently stripped after the call.
    ///
    /// Go's `json.Encoder.Encode` appends a **newline** to every value it writes, which
    /// `json.Marshal` does not. Reproduced — a caller framing responses on that newline would
    /// otherwise block.
    pub fn encode_json<W: std::io::Write>(&mut self, w: &mut W) -> Result<(), EncodeJsonError> {
        self.strip_action_integrations();
        let mut encoded = go_json_marshal(self)?;
        encoded.push('\n');
        w.write_all(encoded.as_bytes())?;
        Ok(())
    }

    /// Port of `(*Post).IsValid` (post.go:500).
    ///
    /// `max_post_size` is a parameter in Go too — it comes from the server config, not from a
    /// constant. [`POST_MESSAGE_MAX_RUNES_V1`] is the usual value.
    pub fn is_valid(&self, max_post_size: usize) -> AppResult {
        let id_detail = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(error("id", None, String::new()));
        }

        if self.create_at == 0 {
            return Err(error("create_at", None, id_detail()));
        }

        if self.update_at == 0 {
            return Err(error("update_at", None, id_detail()));
        }

        if !is_valid_id(&self.user_id) {
            return Err(error("user_id", None, String::new()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(error("channel_id", None, String::new()));
        }

        if !(is_valid_id(&self.root_id) || self.root_id.is_empty()) {
            return Err(error("root_id", None, String::new()));
        }

        // Length only, and in BYTES — `original_id` is never checked against the id alphabet,
        // so 26 exclamation marks pass and 13 two-byte characters do too.
        if !(self.original_id.len() == 26 || self.original_id.is_empty()) {
            return Err(error("original_id", None, String::new()));
        }

        let message_runes = self.message.chars().count();
        if message_runes > max_post_size {
            let params = HashMap::from([
                ("Length".to_string(), serde_json::json!(message_runes)),
                ("MaxLength".to_string(), serde_json::json!(max_post_size)),
            ]);
            return Err(error("message_length", Some(params), id_detail()));
        }

        if self.hashtags.chars().count() > POST_HASHTAGS_MAX_RUNES {
            return Err(error("hashtags", None, id_detail()));
        }

        if !VALID_POST_TYPES.contains(&self.post_type.as_str())
            && !self.post_type.starts_with(POST_CUSTOM_TYPE_PREFIX)
        {
            // The detail carries the *type*, not the post id, despite saying `id=`.
            return Err(error("type", None, format!("id={}", self.post_type)));
        }

        if array_to_json(Some(&self.filenames)).chars().count() > POST_FILENAMES_MAX_RUNES {
            return Err(error("filenames", None, id_detail()));
        }

        if array_to_json(self.file_ids.as_deref()).chars().count() > POST_FILEIDS_MAX_RUNES {
            return Err(error("file_ids", None, id_detail()));
        }

        if string_interface_to_json(self.props.as_ref())
            .chars()
            .count()
            > POST_PROPS_MAX_RUNES
        {
            return Err(error("props", None, id_detail()));
        }

        Ok(())
    }

    /// Port of `(*Post).SanitizeProps` (post.go:596).
    ///
    /// Strips `add_channel_member` always, and the two notification-policy markers unless the
    /// post arrived through Shared Channels federation — the origin cluster already enforced
    /// its own integration-prop authority there. The `from_*` identity markers are render hints
    /// and are **not** stripped.
    pub fn sanitize_props(&mut self) {
        let is_federated = self.remote_id.as_deref().is_some_and(|r| !r.is_empty());

        let mut members_to_sanitize: Vec<&str> = vec![PROPS_ADD_CHANNEL_MEMBER];
        if !is_federated {
            members_to_sanitize.push(POST_PROPS_FORCE_NOTIFICATION);
            members_to_sanitize.push(POST_PROPS_SILENT_NOTIFICATION);
        }

        for member in members_to_sanitize {
            if self.props.as_ref().is_some_and(|p| p.contains_key(member)) {
                self.del_prop(member);
            }
        }

        for p in self.participants.iter_mut().flatten() {
            p.sanitize(&std::collections::HashMap::new());
        }
    }

    /// Port of `(*Post).PreserveIdentityPropsFrom` (post.go:651).
    ///
    /// A prop stored as an explicit JSON `null` is a nil `any` in Go, so `GetProp` returns nil
    /// and the key is **not** carried over. `Value::Null` reproduces that.
    pub fn preserve_identity_props_from(&mut self, old: &Post) {
        for key in POST_IDENTITY_PROPS_PRESERVED_ON_UPDATE {
            if let Some(v) = old.get_prop(key) {
                let v = v.clone();
                self.add_prop(key, v);
            }
        }
    }

    /// Port of `(*Post).SanitizeInput` (post.go:663) — removes everything a client must not
    /// control. Note `remote_id` becomes `Some("")`, not `None`, so the key stays on the wire.
    pub fn sanitize_input(&mut self) {
        self.delete_at = 0;
        self.remote_id = Some(String::new());

        if let Some(metadata) = self.metadata.as_mut() {
            metadata.embeds = Vec::new();
        }
    }

    /// Port of `(*Post).ContainsIntegrationsReservedProps` (post.go:672).
    pub fn contains_integrations_reserved_props(&self) -> Vec<String> {
        contains_integrations_reserved_props(self.props.as_ref())
    }

    /// Port of `(*Post).PreSave` (post.go:709).
    ///
    /// Mints an id when absent, **always** clears `original_id`, takes `create_at` from the
    /// clock only when it is exactly zero (a negative value survives), forces `update_at` to
    /// equal `create_at` even when the caller set it ahead, and then runs [`Self::pre_commit`].
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.original_id = String::new();

        if self.create_at == 0 {
            self.create_at = get_millis();
        }

        self.update_at = self.create_at;
        self.pre_commit();
    }

    /// Port of `(*Post).PreCommit` (post.go:724).
    ///
    /// [`Self::generate_action_ids`] rewrites `props.attachments` with the decoded attachment
    /// list, so a post carrying attachments does **not** come out of `pre_commit` with the props
    /// it went in with — see that method for what the rewrite normalises away.
    pub fn pre_commit(&mut self) {
        if self.props.is_none() {
            self.set_props(Some(StringInterface::new()));
        }

        if self.filenames.is_empty() {
            self.filenames = Vec::new();
        }

        self.generate_action_ids();

        let file_ids = self.file_ids.get_or_insert_with(Vec::new);
        // Go guards a rare client bug that sends duplicate FileIds. `RemoveDuplicateStrings`
        // sorts as well as de-duplicating, so the stored order is not the submitted one.
        remove_duplicate_strings(file_ids);
    }

    /// Port of `(*Post).MakeNonNil` (post.go:743).
    pub fn make_non_nil(&mut self) {
        if self.props.is_none() {
            self.set_props(Some(StringInterface::new()));
        }
    }

    /// Port of `(*Post).DelProp` (post.go:749).
    ///
    /// Go builds a fresh map and swaps it in rather than deleting in place, so a caller holding
    /// the old map does not observe the removal. `&mut self` makes that unobservable here.
    ///
    /// The assignment is **unconditional**, though, and that half *is* observable: deleting from
    /// a nil `Props` leaves an empty map behind rather than nil. `props` carries no `omitempty`,
    /// so it is the difference between `"props":null` and `"props":{}` on the wire. Reachable
    /// through [`Post::strip_mm_blocks_action_secrets`] and pinned by the oracle.
    pub fn del_prop(&mut self, key: &str) {
        self.props
            .get_or_insert_with(StringInterface::new)
            .remove(key);
    }

    /// Port of `(*Post).AddProp` (post.go:758). Creates the map when absent, as Go does.
    pub fn add_prop(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.props
            .get_or_insert_with(StringInterface::new)
            .insert(key.into(), value);
    }

    /// Port of `(*Post).GetProps` (post.go:767).
    pub fn get_props(&self) -> Option<&StringInterface> {
        self.props.as_ref()
    }

    /// Port of `(*Post).SetProps` (post.go:773).
    pub fn set_props(&mut self, props: Option<StringInterface>) {
        self.props = props;
    }

    /// Port of `(*Post).GetProp` (post.go:779).
    ///
    /// Go's map read on an `any` value cannot distinguish an absent key from one holding a JSON
    /// `null` — both are a nil interface. `get_prop` collapses them the same way, returning
    /// `None` for a stored `Value::Null`, which is what every caller branches on.
    pub fn get_prop(&self, key: &str) -> Option<&serde_json::Value> {
        match self.props.as_ref()?.get(key) {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => Some(v),
        }
    }

    /// Port of `(*Post).HasUnsafeLinks` (post.go:787). Exact string `"true"` only — a real bool
    /// `true` does not count.
    pub fn has_unsafe_links(&self) -> bool {
        self.get_prop(POST_PROPS_UNSAFE_LINKS)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "true")
    }

    /// Port of `(*Post).IsSystemMessage` (post.go:1057).
    pub fn is_system_message(&self) -> bool {
        self.post_type.starts_with(POST_SYSTEM_MESSAGE_PREFIX)
    }

    /// Port of `(*Post).IsAccessControlTeamMembershipNotification` (post.go:1064).
    pub fn is_access_control_team_membership_notification(&self) -> bool {
        self.post_type == POST_TYPE_ACCESS_CONTROL_TEAM_REMOVAL
            || self.post_type == POST_TYPE_ACCESS_CONTROL_TEAM_ADDITION
    }

    /// Port of `(*Post).HasForceNotification` (post.go:1068).
    ///
    /// Go's type switch answers `true` for **any non-empty string**, `"false"` included, and
    /// `false` for every non-bool non-string. Not symmetric with
    /// [`Self::has_silent_notification`]; see the module notes.
    pub fn has_force_notification(&self) -> bool {
        match self.get_prop(POST_PROPS_FORCE_NOTIFICATION) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            _ => false,
        }
    }

    /// Port of `(*Post).HasSilentNotification` (post.go:1079). A real bool and nothing else.
    pub fn has_silent_notification(&self) -> bool {
        matches!(
            self.get_prop(POST_PROPS_SILENT_NOTIFICATION),
            Some(serde_json::Value::Bool(true))
        )
    }

    /// Port of `(*Post).IsNotificationSuppressed` (post.go:1092). Force wins.
    pub fn is_notification_suppressed(&self) -> bool {
        if self.has_force_notification() {
            return false;
        }
        self.has_silent_notification()
    }

    /// Port of `(*Post).ExcludesFromChannelMessageCount` (post.go:1102).
    pub fn excludes_from_channel_message_count(&self) -> bool {
        self.is_join_leave_message() || self.is_notification_suppressed()
    }

    /// Port of `(*Post).IsRemote` (post.go:1107).
    pub fn is_remote(&self) -> bool {
        self.remote_id.as_deref().is_some_and(|r| !r.is_empty())
    }

    /// Port of `(*Post).GetRemoteID` (post.go:1112). Collapses nil and empty to `""`.
    pub fn get_remote_id(&self) -> &str {
        self.remote_id.as_deref().unwrap_or("")
    }

    /// Port of `(*Post).IsJoinLeaveMessage` (post.go:1119).
    pub fn is_join_leave_message(&self) -> bool {
        matches!(
            self.post_type.as_str(),
            POST_TYPE_JOIN_LEAVE
                | POST_TYPE_ADD_REMOVE
                | POST_TYPE_JOIN_CHANNEL
                | POST_TYPE_LEAVE_CHANNEL
                | POST_TYPE_JOIN_TEAM
                | POST_TYPE_LEAVE_TEAM
                | POST_TYPE_ADD_TO_CHANNEL
                | POST_TYPE_REMOVE_FROM_CHANNEL
                | POST_TYPE_ADD_TO_TEAM
                | POST_TYPE_REMOVE_FROM_TEAM
        )
    }

    /// Port of `(*Post).Patch` (post.go:1132).
    ///
    /// Props and file ids are **replaced wholesale**, not merged, and nothing is trimmed —
    /// unlike `Channel::patch`, which trims `display_name`.
    pub fn patch(&mut self, patch: &PostPatch) {
        if let Some(is_pinned) = patch.is_pinned {
            self.is_pinned = is_pinned;
        }

        if let Some(message) = patch.message.as_ref() {
            self.message.clone_from(message);
        }

        if let Some(props) = patch.props.as_ref() {
            self.set_props(Some(props.clone()));
        }

        if let Some(file_ids) = patch.file_ids.as_ref() {
            self.file_ids = Some(file_ids.clone());
        }

        if let Some(has_reactions) = patch.has_reactions {
            self.has_reactions = has_reactions;
        }
    }

    /// Port of `(*Post).DisableMentionHighlights` (post.go:1174). Returns the first `@channel`,
    /// `@all` or `@here` mention found, lowercased, and sets the disabling prop when there was
    /// one.
    pub fn disable_mention_highlights(&mut self) -> Option<String> {
        let mention = find_at_channel_mention(&self.message)?;
        self.add_prop(
            POST_PROPS_MENTION_HIGHLIGHT_DISABLED,
            serde_json::Value::Bool(true),
        );
        Some(mention)
    }

    /// Port of `(*Post).IsFromOAuthBot` (post.go:1343).
    ///
    /// Go compares two `any` values against strings. An **absent** `override_username` is a nil
    /// interface and `nil != ""` is true, so the second half of the conjunction is satisfied by
    /// a prop that was never set. Reproduced; see the module notes.
    pub fn is_from_oauth_bot(&self) -> bool {
        let from_webhook_is_true = self
            .get_prop(POST_PROPS_FROM_WEBHOOK)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "true");

        let username_is_not_empty = match self.get_prop(POST_PROPS_OVERRIDE_USERNAME) {
            // Absent, or a stored null: a nil `any`, which is `!= ""`.
            None => true,
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            // Any other type is also `!= ""`.
            Some(_) => true,
        };

        from_webhook_is_true && username_is_not_empty
    }

    /// Port of `(*Post).ToNilIfInvalid` (post.go:1348). `None` when the post has no id.
    pub fn to_nil_if_invalid(&self) -> Option<&Post> {
        if self.id.is_empty() {
            return None;
        }
        Some(self)
    }

    /// Port of `(*Post).ForPlugin` (post.go:1355).
    ///
    /// Ported ahead of its chunk because `(*PostList).ForPlugin` (post_list.go:58) is a
    /// three-line wrapper around it and would otherwise be the only unported method in
    /// `post_list.go` with no dependency worth deferring for.
    ///
    /// Drops the metadata, and for the one type `custom_up_notification` also drops the
    /// `requested_features` prop — which materialises a nil `props` into `{}`, because
    /// [`Self::del_prop`] does.
    pub fn for_plugin(&self) -> Post {
        let mut copy = self.clone();
        copy.metadata = None;
        if copy.post_type == format!("{POST_CUSTOM_TYPE_PREFIX}up_notification") {
            copy.del_prop("requested_features");
        }
        copy
    }

    /// Port of `(*Post).GetPreviewedPostProp` (post.go:1378). A non-string value yields `""`.
    pub fn get_previewed_post_prop(&self) -> &str {
        self.get_prop(POST_PROPS_PREVIEWED_POST)
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// Port of `(*Post).GetPriority` (post.go:1385).
    pub fn get_priority(&self) -> Option<&PostPriority> {
        self.metadata.as_ref()?.priority.as_ref()
    }

    /// Port of `(*Post).GetPersistentNotification` (post.go:1392).
    pub fn get_persistent_notification(&self) -> Option<bool> {
        self.get_priority()?.persistent_notifications
    }

    /// Port of `(*Post).GetRequestedAck` (post.go:1400).
    pub fn get_requested_ack(&self) -> Option<bool> {
        self.get_priority()?.requested_ack
    }

    /// Port of `(*Post).IsUrgent` (post.go:1408).
    pub fn is_urgent(&self) -> bool {
        self.get_priority()
            .and_then(|p| p.priority.as_deref())
            .is_some_and(|p| p == POST_PRIORITY_URGENT)
    }

    /// Port of `(*Post).CleanPost` (post.go:1421).
    ///
    /// Clears the four identity/time fields Go lists and **not** `delete_at`, which is easy to
    /// assume is in the set.
    pub fn clean_post(&mut self) -> &mut Self {
        self.id = String::new();
        self.create_at = 0;
        self.update_at = 0;
        self.edit_at = 0;
        self
    }

    /// Port of `(*Post).Attachments` (post.go:1204).
    ///
    /// Go's first branch asserts that `props.attachments` already holds a
    /// `[]*MessageAttachment` — a Go-native value only an in-process caller can have stored,
    /// and unreachable here because props are `serde_json::Value`. The reachable branch is a
    /// **re-decode**: Go marshals each element and unmarshals it into a `MessageAttachment`,
    /// dropping the element when either step fails.
    ///
    /// Four results are not obvious from the source and are pinned by the oracle:
    ///
    /// - a bare `null` element is **not** dropped. `json.Unmarshal("null", &struct)` leaves the
    ///   destination untouched and reports no error, so it contributes a *zero* attachment.
    /// - one wrongly-typed key drops the whole element, so `{"title": 123}` disappears while
    ///   its neighbours survive.
    /// - nil elements of `actions` and `fields` are stripped. A nil **option** inside an action
    ///   is not, and Go keeps the attachment; we cannot decode it at all, so we drop the whole
    ///   attachment — see [D-033].
    /// - Go's `encoding/json` matches keys case-insensitively and serde does not, so
    ///   `{"Title":"t"}` is a titled attachment for Go and an empty one for us — see [D-040].
    pub fn attachments(&self) -> Vec<MessageAttachment> {
        let Some(serde_json::Value::Array(elements)) = self.get_prop(POST_PROPS_ATTACHMENTS) else {
            return Vec::new();
        };

        let mut ret = Vec::new();
        for element in elements {
            if element.is_null() {
                ret.push(MessageAttachment::default());
                continue;
            }
            // serde's derived `Deserialize` accepts a **sequence** as a struct, taking the
            // fields in declaration order; Go's `encoding/json` accepts only an object. Without
            // this guard `[[]]` would decode into a zero attachment for us and be dropped by
            // Go — pinned by the `element_array` case.
            if !element.is_object() {
                continue;
            }
            // Go re-marshals every element before decoding it; this owned copy is that same
            // round trip, and it is what lets the nil filter run against the JSON. Filtering
            // before the decode rather than after is equivalent — a nil element cannot be the
            // reason a decode fails in Go, since `[]*T` accepts one.
            let mut element = element.clone();
            strip_nil_elements(&mut element, "actions");
            strip_nil_elements(&mut element, "fields");
            if let Ok(decoded) = serde_json::from_value::<MessageAttachment>(element) {
                ret.push(decoded);
            }
        }
        ret
    }

    /// Port of `(*Post).AttachmentsEqual` (post.go:1241).
    ///
    /// Both sides go through [`Self::attachments`] first, so an element Go would have dropped is
    /// absent rather than unequal: a post whose only attachment is malformed compares **equal**
    /// to a post with no attachments at all.
    ///
    /// Go panics here for most real inputs. `MessageAttachmentField::equals` reflects on a nil
    /// `Value`, and a field with no `value` key decodes to exactly that; ours compares
    /// `Value::Null` normally. See [D-039].
    pub fn attachments_equal(&self, input: &Post) -> bool {
        let ours = self.attachments();
        let theirs = input.attachments();

        if ours.len() != theirs.len() {
            return false;
        }
        ours.iter().zip(theirs.iter()).all(|(a, b)| a.equals(b))
    }

    /// Port of `(*Post).AllStrings` (post.go:806).
    ///
    /// The output is the message, then each attachment's author name, title, text, pretext and
    /// footer, then each field's title and value, and finally — unless
    /// `omit_interactive_blocks` is set — the human-readable text of `props.mm_blocks`,
    /// `props.blocks` and `props.cards`, in that order. Two asymmetries are load-bearing:
    ///
    /// - a value that **is** a string is appended with its original bytes (padding included) as
    ///   long as it is not whitespace-only; any other value is rendered with Go's `%v` and
    ///   appended **trimmed**.
    /// - a nil value is skipped entirely, so a field with no `value` key contributes only its
    ///   title.
    pub fn all_strings(&self, opts: AllStringsOptions) -> Vec<String> {
        let mut out = Vec::new();
        append_non_whitespace_only_message(&mut out, &self.message);

        for attachment in self.attachments() {
            append_non_whitespace_only_message(&mut out, &attachment.author_name);
            append_non_whitespace_only_message(&mut out, &attachment.title);
            append_non_whitespace_only_message(&mut out, &attachment.text);
            append_non_whitespace_only_message(&mut out, &attachment.pretext);
            append_non_whitespace_only_message(&mut out, &attachment.footer);

            let Some(fields) = attachment.fields.as_ref() else {
                continue;
            };
            for field in fields {
                append_non_whitespace_only_message(&mut out, &field.title);
                match &field.value {
                    serde_json::Value::Null => continue,
                    serde_json::Value::String(s) => {
                        append_non_whitespace_only_message(&mut out, s);
                    }
                    other => {
                        let rendered = go_format_v(other);
                        let trimmed = rendered.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                }
            }
        }

        if !opts.omit_interactive_blocks {
            append_human_readable_interactive_strings(self, &mut out);
        }
        out
    }

    /// Port of `(*Post).InteractiveBlocksImageURLs` (post.go:846).
    ///
    /// Non-markdown image URLs from the three interactive dialects plus the four image fields of
    /// every attachment. `mm_blocks_enabled` gates **all three** dialects — Block Kit and
    /// Adaptive Cards included — while attachment URLs are collected either way.
    ///
    /// Markdown `![alt](url)` inside interactive text is deliberately *not* collected here; Go's
    /// comment says callers merge those from [`Self::all_strings`] separately. Link-preview
    /// policy is likewise the caller's.
    pub fn interactive_blocks_image_urls(&self, mm_blocks_enabled: bool) -> Vec<String> {
        interactive_blocks_image_urls(self, mm_blocks_enabled)
    }
}

/// Port of `model.AllStringsOptions` (post.go:796). Not a wire type — no `json:` tags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllStringsOptions {
    pub omit_interactive_blocks: bool,
}

/// Port of `appendNonWhitespaceOnlyMessage` (post.go:866).
///
/// Appends the string **unmodified** unless `strings.TrimSpace` would empty it. Go's
/// `unicode.IsSpace` and Rust's `char::is_whitespace` are both the Unicode White_Space property,
/// which the oracle confirms over NBSP, the ideographic and ogham spaces, NEL, U+180E and the
/// zero-width space — the last two are *not* whitespace in either language.
pub(crate) fn append_non_whitespace_only_message(out: &mut Vec<String>, s: &str) {
    if s.trim().is_empty() {
        return;
    }
    out.push(s.to_string());
}

/// Removes `null` elements from `object[key]` when it holds an array, mirroring the nil filter
/// Go applies to a decoded attachment's `actions` and `fields`. A value that is not an array is
/// left alone, so a wrongly-typed key still fails the decode exactly as it does in Go.
fn strip_nil_elements(object: &mut serde_json::Value, key: &str) {
    if let Some(serde_json::Value::Array(items)) = object.get_mut(key) {
        items.retain(|item| !item.is_null());
    }
}

/// Port of `model.ContainsIntegrationsReservedProps` (post.go:683).
///
/// Membership, not truthiness — a key present with a `null` value still counts. The result is
/// ordered by [`INTEGRATIONS_RESERVED_PROPS`], never by the caller's map.
pub fn contains_integrations_reserved_props(props: Option<&StringInterface>) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(props) = props {
        for key in INTEGRATIONS_RESERVED_PROPS {
            if props.contains_key(key) {
                found.push(key.to_string());
            }
        }
    }
    found
}

/// Port of `findAtChannelMention` (post.go:1195).
///
/// Go compiles `(?i)\B@(channel|all|here)\b` and lowercases the whole match. The `\B` means the
/// `@` must not sit on a word boundary, so `a@channel` does not match while `-@channel` does;
/// the `\b` means `@channel_` does not match while `@channel-` does.
pub fn find_at_channel_mention(message: &str) -> Option<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        #[allow(clippy::expect_used)]
        regex::Regex::new(r"(?i)\B@(channel|all|here)\b").expect("literal regex is valid")
    });
    RE.find(message).map(|m| go_to_lower(m.as_str()))
}

// --- PostPatch ------------------------------------------------------------------------------

/// Port of `model.PostPatch` (post.go:211). No field carries `omitempty`, so every key is
/// always present and `null` when unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostPatch {
    #[serde(rename = "is_pinned")]
    pub is_pinned: Option<bool>,

    #[serde(rename = "message")]
    pub message: Option<String>,

    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "file_ids")]
    pub file_ids: Option<StringArray>,

    #[serde(rename = "has_reactions")]
    pub has_reactions: Option<bool>,
}

impl PostPatch {
    /// Port of `(*PostPatch).IsEmpty` (post.go:219). A pointer to a *zero value* is still set,
    /// so a patch of `{"is_pinned": false}` is not empty.
    pub fn is_empty(&self) -> bool {
        self.is_pinned.is_none()
            && self.message.is_none()
            && self.props.is_none()
            && self.file_ids.is_none()
            && self.has_reactions.is_none()
    }

    /// Port of `(*PostPatch).ContainsIntegrationsReservedProps` (post.go:676).
    ///
    /// Go returns a nil slice for a nil receiver *or* nil props and an empty slice otherwise;
    /// the two are indistinguishable once marshalled, so `Vec` carries both.
    pub fn contains_integrations_reserved_props(&self) -> Vec<String> {
        contains_integrations_reserved_props(self.props.as_ref())
    }

    /// Port of `(*PostPatch).DisableMentionHighlights` (post.go:1183). A patch with no message
    /// is left alone entirely — including its props.
    pub fn disable_mention_highlights(&mut self) {
        let Some(message) = self.message.as_ref() else {
            return;
        };
        if find_at_channel_mention(message).is_some() {
            self.props.get_or_insert_with(StringInterface::new).insert(
                POST_PROPS_MENTION_HIGHLIGHT_DISABLED.to_string(),
                serde_json::Value::Bool(true),
            );
        }
    }
}

// --- the small satellite types --------------------------------------------------------------

/// Port of `model.PostEphemeral` (post.go:206).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostEphemeral {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "post")]
    pub post: Option<Post>,
}

/// Port of `model.PostReminder` (post.go:223).
///
/// `PostId` and `UserId` are tagged `json:",omitempty"` — an **empty name**, so Go falls back to
/// the field name and two capitalised keys sit beside the snake_case one. Same trap as
/// `PostPriority` and `TeamForExport.SchemeName`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostReminder {
    #[serde(rename = "target_time")]
    pub target_time: i64,

    #[serde(rename = "PostId", default, skip_serializing_if = "String::is_empty")]
    pub post_id: String,

    #[serde(rename = "UserId", default, skip_serializing_if = "String::is_empty")]
    pub user_id: String,
}

/// Port of `model.MoveThreadParams` (post.go:253).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveThreadParams {
    #[serde(rename = "channel_id")]
    pub channel_id: String,
}

/// Port of `model.SearchParameter` (post.go:257).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchParameter {
    #[serde(rename = "terms")]
    pub terms: Option<String>,

    #[serde(rename = "is_or_search")]
    pub is_or_search: Option<bool>,

    #[serde(rename = "time_zone_offset")]
    pub time_zone_offset: Option<i64>,

    #[serde(rename = "page")]
    pub page: Option<i64>,

    #[serde(rename = "per_page")]
    pub per_page: Option<i64>,

    #[serde(rename = "include_deleted_channels")]
    pub include_deleted_channels: Option<bool>,
}

/// Port of `model.PostForIndexing` (post.go:326). Embeds `Post`, whose fields are inlined.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostForIndexing {
    #[serde(flatten)]
    pub post: Post,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "parent_create_at")]
    pub parent_create_at: Option<i64>,

    #[serde(rename = "channel_type")]
    pub channel_type: String,
}

/// Port of `model.FileForIndexing` (post.go:333).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileForIndexing {
    #[serde(flatten)]
    pub file_info: crate::file_info::FileInfo,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "content")]
    pub content: String,
}

impl FileForIndexing {
    /// Port of `(*FileForIndexing).ShouldIndex` (post.go:348).
    ///
    /// Go's receiver is nilable and the method starts with `file != nil`; a Rust `&self` cannot
    /// be null, so the caller-side `Option` carries that case — see the `go_parity` test.
    pub fn should_index(&self) -> bool {
        self.file_info.delete_at == 0
            && (!self.file_info.post_id.is_empty()
                || self.file_info.creator_id == crate::file_info::BOOKMARK_FILE_OWNER)
    }
}

/// Port of `model.GetPostsSinceForSyncCursor` (post.go:436). A store cursor, not a wire type:
/// no field carries a `json:` tag.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GetPostsSinceForSyncCursor {
    pub last_post_update_at: i64,
    pub last_post_update_id: String,
    pub last_post_create_at: i64,
    pub last_post_create_id: String,
}

impl GetPostsSinceForSyncCursor {
    /// Port of `(GetPostsSinceForSyncCursor).IsEmpty` (post.go:443).
    pub fn is_empty(&self) -> bool {
        self.last_post_create_at == 0
            && self.last_post_create_id.is_empty()
            && self.last_post_update_at == 0
            && self.last_post_update_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_post() -> Post {
        Post {
            id: "6bdz674pgq767e4jx75w4pf57a".into(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_001_000,
            user_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            channel_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
            ..Default::default()
        }
    }

    /// Go leaves an absent field at its zero value, so the payload a client actually posts —
    /// a channel id and a message — must decode. Without `#[serde(default)]` on the container it
    /// fails with `missing field 'id'`. See [D-043].
    #[test]
    fn a_partial_post_decodes_the_way_go_zero_fills() {
        let post: Post = serde_json::from_str(r#"{"channel_id":"c","message":"hi"}"#).unwrap();
        assert_eq!(post.channel_id, "c");
        assert_eq!(post.message, "hi");
        assert_eq!(post.id, "");
        assert_eq!(post.create_at, 0);
        assert!(!post.is_pinned);
        assert_eq!(post.props, None);

        let empty: Post = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Post::default());
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/post.json");
        let original: serde_json::Value = serde_json::from_str(raw).unwrap();
        let post: Post = serde_json::from_str(raw).unwrap();
        let again = serde_json::to_value(&post).unwrap();
        assert_eq!(original, again);
    }

    #[test]
    fn the_fixture_covers_every_wire_key() {
        let raw = include_str!("../../../fixtures/post.json");
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let obj = v.as_object().unwrap();
        // `filenames` is json:"-" and must NOT be here; everything else must.
        assert!(!obj.contains_key("filenames"));
        for key in [
            "id",
            "create_at",
            "update_at",
            "edit_at",
            "delete_at",
            "is_pinned",
            "user_id",
            "channel_id",
            "root_id",
            "original_id",
            "message",
            "message_source",
            "type",
            "props",
            "hashtags",
            "file_ids",
            "pending_post_id",
            "has_reactions",
            "remote_id",
            "reply_count",
            "last_reply_at",
            "participants",
            "is_following",
            "metadata",
        ] {
            assert!(obj.contains_key(key), "fixture is missing {key}");
        }
    }

    #[test]
    fn props_serialise_in_sorted_key_order_like_go() {
        let mut post = Post::default();
        post.add_prop("z", json!(1));
        post.add_prop("a", json!(2));
        post.add_prop("M", json!(3));
        let s = serde_json::to_string(&post).unwrap();
        assert!(s.contains(r#""props":{"M":3,"a":2,"z":1}"#), "{s}");
    }

    #[test]
    fn get_prop_collapses_absent_and_stored_null() {
        let mut post = Post::default();
        post.add_prop("a", serde_json::Value::Null);
        assert!(post.get_prop("a").is_none());
        assert!(post.get_prop("missing").is_none());
        // The key is still *present* — only the read collapses.
        assert!(post.props.as_ref().unwrap().contains_key("a"));
        assert_eq!(
            contains_integrations_reserved_props(post.props.as_ref()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn add_prop_creates_the_map() {
        let mut post = Post::default();
        assert!(post.props.is_none());
        post.add_prop("a", json!("b"));
        assert_eq!(post.get_prop("a"), Some(&json!("b")));
    }

    /// Go's `DelProp` assigns its fresh map unconditionally, so a nil `Props` comes out as an
    /// **empty** map — which `props` (no `omitempty`) then writes as `{}` rather than `null`.
    /// This test previously asserted the opposite; the oracle's `del_prop` section corrected it.
    #[test]
    fn del_prop_on_a_nil_map_materialises_it() {
        let mut post = Post::default();
        post.del_prop("a");
        assert_eq!(post.props, Some(StringInterface::new()));
    }

    #[test]
    fn is_valid_accepts_a_minimal_post() {
        assert!(valid_post().is_valid(POST_MESSAGE_MAX_RUNES_V1).is_ok());
    }

    #[test]
    fn is_valid_rejects_a_bad_id() {
        let mut post = valid_post();
        post.id = String::new();
        let err = post.is_valid(POST_MESSAGE_MAX_RUNES_V1).unwrap_err();
        assert_eq!(err.id, "model.post.is_valid.id.app_error");
    }

    #[test]
    fn pre_save_mints_an_id_and_clears_original_id() {
        let mut post = Post {
            original_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            create_at: 100,
            update_at: 999,
            ..Default::default()
        };
        post.pre_save();
        assert_eq!(post.id.len(), 26);
        assert_eq!(post.original_id, "");
        assert_eq!(post.create_at, 100);
        assert_eq!(post.update_at, 100);
        assert!(post.props.is_some());
        assert!(post.file_ids.is_some());
    }

    #[test]
    fn pre_commit_sorts_and_dedups_file_ids() {
        let mut post = Post {
            file_ids: Some(vec!["c".into(), "a".into(), "c".into(), "b".into()]),
            ..Default::default()
        };
        post.pre_commit();
        assert_eq!(
            post.file_ids.as_deref(),
            Some(&["a".to_string(), "b".into(), "c".into()][..])
        );
    }

    #[test]
    fn patch_replaces_props_wholesale() {
        let mut post = Post::default();
        post.add_prop("old", json!(1));
        let mut new_props = StringInterface::new();
        new_props.insert("new".into(), json!(2));
        post.patch(&PostPatch {
            props: Some(new_props),
            ..Default::default()
        });
        assert!(post.get_prop("old").is_none());
        assert_eq!(post.get_prop("new"), Some(&json!(2)));
    }

    #[test]
    fn sanitize_input_materialises_an_empty_remote_id() {
        let mut post = Post {
            delete_at: 5,
            remote_id: Some("cluster-a".into()),
            ..Default::default()
        };
        post.sanitize_input();
        assert_eq!(post.delete_at, 0);
        assert_eq!(post.remote_id.as_deref(), Some(""));
        // The key stays on the wire, unlike a None.
        let s = serde_json::to_string(&post).unwrap();
        assert!(s.contains(r#""remote_id":"""#), "{s}");
    }
}

/// Tests that assert against `fixtures/behaviour_post.json`, produced by
/// `reference/dump/behaviour_post.go`. Every expectation here is Go's measured answer, not a
/// reading of the Go source.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_post.json")).unwrap()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    /// Rebuilds a `Post` from the Go-marshalled JSON the oracle recorded, so a wire drift and a
    /// logic drift fail the same test.
    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_value(v.get(key).unwrap().clone()).unwrap()
    }

    #[test]
    fn the_constants_match_go() {
        let o = oracle();
        let c = o.get("constants").unwrap();

        for (key, ours) in [
            ("post_system_message_prefix", POST_SYSTEM_MESSAGE_PREFIX),
            ("post_type_default", POST_TYPE_DEFAULT),
            ("post_type_message_attachment", POST_TYPE_MESSAGE_ATTACHMENT),
            ("post_type_system_generic", POST_TYPE_SYSTEM_GENERIC),
            ("post_type_join_leave", POST_TYPE_JOIN_LEAVE),
            ("post_type_join_channel", POST_TYPE_JOIN_CHANNEL),
            ("post_type_guest_join_channel", POST_TYPE_GUEST_JOIN_CHANNEL),
            ("post_type_leave_channel", POST_TYPE_LEAVE_CHANNEL),
            ("post_type_join_team", POST_TYPE_JOIN_TEAM),
            ("post_type_leave_team", POST_TYPE_LEAVE_TEAM),
            ("post_type_auto_responder", POST_TYPE_AUTO_RESPONDER),
            (
                "post_type_autotranslation_change",
                POST_TYPE_AUTOTRANSLATION_CHANGE,
            ),
            ("post_type_add_remove", POST_TYPE_ADD_REMOVE),
            ("post_type_add_to_channel", POST_TYPE_ADD_TO_CHANNEL),
            (
                "post_type_add_guest_to_channel",
                POST_TYPE_ADD_GUEST_TO_CHANNEL,
            ),
            (
                "post_type_remove_from_channel",
                POST_TYPE_REMOVE_FROM_CHANNEL,
            ),
            ("post_type_move_channel", POST_TYPE_MOVE_CHANNEL),
            ("post_type_add_to_team", POST_TYPE_ADD_TO_TEAM),
            ("post_type_remove_from_team", POST_TYPE_REMOVE_FROM_TEAM),
            (
                "post_type_access_control_team_removal",
                POST_TYPE_ACCESS_CONTROL_TEAM_REMOVAL,
            ),
            (
                "post_type_access_control_team_add",
                POST_TYPE_ACCESS_CONTROL_TEAM_ADDITION,
            ),
            ("post_type_header_change", POST_TYPE_HEADER_CHANGE),
            ("post_type_displayname_change", POST_TYPE_DISPLAYNAME_CHANGE),
            ("post_type_convert_channel", POST_TYPE_CONVERT_CHANNEL),
            ("post_type_purpose_change", POST_TYPE_PURPOSE_CHANGE),
            ("post_type_channel_deleted", POST_TYPE_CHANNEL_DELETED),
            ("post_type_channel_restored", POST_TYPE_CHANNEL_RESTORED),
            ("post_type_ephemeral", POST_TYPE_EPHEMERAL),
            (
                "post_type_change_channel_privacy",
                POST_TYPE_CHANGE_CHANNEL_PRIVACY,
            ),
            ("post_type_wrangler", POST_TYPE_WRANGLER),
            (
                "post_type_gm_converted_to_channel",
                POST_TYPE_GM_CONVERTED_TO_CHANNEL,
            ),
            (
                "post_type_add_bot_teams_channels",
                POST_TYPE_ADD_BOT_TEAMS_CHANNELS,
            ),
            ("post_type_me", POST_TYPE_ME),
            ("post_custom_type_prefix", POST_CUSTOM_TYPE_PREFIX),
            ("post_type_reminder", POST_TYPE_REMINDER),
            ("post_type_burn_on_read", POST_TYPE_BURN_ON_READ),
            ("post_type_card", POST_TYPE_CARD),
            (
                "post_type_shared_channel_state",
                POST_TYPE_SHARED_CHANNEL_STATE,
            ),
            (
                "reporting_time_field_create_at",
                REPORTING_TIME_FIELD_CREATE_AT,
            ),
            (
                "reporting_time_field_update_at",
                REPORTING_TIME_FIELD_UPDATE_AT,
            ),
            ("reporting_sort_direction_asc", REPORTING_SORT_DIRECTION_ASC),
            (
                "reporting_sort_direction_desc",
                REPORTING_SORT_DIRECTION_DESC,
            ),
            ("props_add_channel_member", PROPS_ADD_CHANNEL_MEMBER),
            ("post_props_added_user_id", POST_PROPS_ADDED_USER_ID),
            ("post_props_delete_by", POST_PROPS_DELETE_BY),
            ("post_props_override_icon_url", POST_PROPS_OVERRIDE_ICON_URL),
            (
                "post_props_override_icon_emoji",
                POST_PROPS_OVERRIDE_ICON_EMOJI,
            ),
            ("post_props_override_username", POST_PROPS_OVERRIDE_USERNAME),
            ("post_props_from_webhook", POST_PROPS_FROM_WEBHOOK),
            ("post_props_from_bot", POST_PROPS_FROM_BOT),
            ("post_props_from_oauth_app", POST_PROPS_FROM_OAUTH_APP),
            (
                "post_props_webhook_display_name",
                POST_PROPS_WEBHOOK_DISPLAY_NAME,
            ),
            ("post_props_from_plugin", POST_PROPS_FROM_PLUGIN),
            (
                "post_props_mention_highlight_disabl",
                POST_PROPS_MENTION_HIGHLIGHT_DISABLED,
            ),
            (
                "post_props_group_highlight_disabled",
                POST_PROPS_GROUP_HIGHLIGHT_DISABLED,
            ),
            ("post_props_previewed_post", POST_PROPS_PREVIEWED_POST),
            (
                "post_props_force_notification",
                POST_PROPS_FORCE_NOTIFICATION,
            ),
            (
                "post_props_silent_notification",
                POST_PROPS_SILENT_NOTIFICATION,
            ),
            ("post_props_channel_mentions", POST_PROPS_CHANNEL_MENTIONS),
            ("post_props_current_team_id", POST_PROPS_CURRENT_TEAM_ID),
            ("post_props_unsafe_links", POST_PROPS_UNSAFE_LINKS),
            (
                "post_props_ai_generated_by_user_id",
                POST_PROPS_AI_GENERATED_BY_USER_ID,
            ),
            (
                "post_props_ai_generated_by_username",
                POST_PROPS_AI_GENERATED_BY_USERNAME,
            ),
            ("post_props_expire_at", POST_PROPS_EXPIRE_AT),
            (
                "post_props_read_duration_seconds",
                POST_PROPS_READ_DURATION_SECONDS,
            ),
            (
                "post_props_shared_channel_state",
                POST_PROPS_SHARED_CHANNEL_STATE,
            ),
            (
                "post_props_shared_channel_workspace",
                POST_PROPS_SHARED_CHANNEL_WORKSPACE_NAME,
            ),
            ("post_props_attachments", POST_PROPS_ATTACHMENTS),
            ("post_props_mm_blocks", POST_PROPS_MM_BLOCKS),
            ("post_props_block_kit_blocks", POST_PROPS_BLOCK_KIT_BLOCKS),
            ("post_props_adaptive_cards", POST_PROPS_ADAPTIVE_CARDS),
            ("post_props_mm_blocks_actions", POST_PROPS_MM_BLOCKS_ACTIONS),
            ("post_priority_urgent", POST_PRIORITY_URGENT),
            (
                "post_context_key_is_scheduled",
                POST_CONTEXT_KEY_IS_SCHEDULED_POST,
            ),
            (
                "shared_channel_state_shared",
                SHARED_CHANNEL_STATE_POST_VALUE_SHARED,
            ),
            (
                "shared_channel_state_unshared",
                SHARED_CHANNEL_STATE_POST_VALUE_UNSHARED,
            ),
        ] {
            assert_eq!(s(c, key), ours, "constant {key}");
        }

        for (key, ours) in [
            ("post_fileids_max_runes", POST_FILEIDS_MAX_RUNES as i64),
            ("post_filenames_max_runes", POST_FILENAMES_MAX_RUNES as i64),
            ("post_hashtags_max_runes", POST_HASHTAGS_MAX_RUNES as i64),
            (
                "post_message_max_runes_v1",
                POST_MESSAGE_MAX_RUNES_V1 as i64,
            ),
            (
                "post_message_max_bytes_v2",
                POST_MESSAGE_MAX_BYTES_V2 as i64,
            ),
            (
                "post_message_max_runes_v2",
                POST_MESSAGE_MAX_RUNES_V2 as i64,
            ),
            ("max_reporting_per_page", MAX_REPORTING_PER_PAGE),
            ("post_props_max_runes", POST_PROPS_MAX_RUNES as i64),
            (
                "post_props_max_user_runes",
                POST_PROPS_MAX_USER_RUNES as i64,
            ),
            ("default_expiry_seconds", DEFAULT_EXPIRY_SECONDS),
            (
                "default_read_duration_seconds",
                DEFAULT_READ_DURATION_SECONDS,
            ),
            (
                "post_identity_props_on_update_n",
                POST_IDENTITY_PROPS_PRESERVED_ON_UPDATE.len() as i64,
            ),
        ] {
            assert_eq!(
                c.get(key).unwrap().as_i64().unwrap(),
                ours,
                "constant {key}"
            );
        }
    }

    /// Byte-exact, through [`crate::utils::go_json_marshal`]. Field order, `omitempty` on each
    /// shape, `null` for the three nil-able collections, and Go's **sorted** props keys all
    /// fail here if they drift.
    ///
    /// `serde_json::to_string` is deliberately not what is compared: it does not apply Go's
    /// HTML escaping, so a prop containing `<`, `>`, `&`, U+2028 or U+2029 differs by bytes
    /// while decoding to the same value ([D-022]). The plain-serde graph is checked separately
    /// below so both facts are pinned rather than one of them hidden.
    #[test]
    fn the_wire_format_matches_go() {
        let o = oracle();
        for case in o.get("wire").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let go_json = s(case, "json");
            let go_roundtrip = s(case, "roundtrip");

            let post: Post = serde_json::from_str(&go_json).unwrap();
            let ours = crate::utils::go_json_marshal(&post).unwrap();
            assert_eq!(
                ours, go_roundtrip,
                "round trip differs from Go's for {name}"
            );
            // Nothing in this corpus is lossy in Go, so its output and its round trip agree.
            assert_eq!(go_json, go_roundtrip, "oracle self-check for {name}");

            // Plain serde decodes to the same *value* everywhere, escaping aside.
            let plain: serde_json::Value = serde_json::to_value(&post).unwrap();
            let go_value: serde_json::Value = serde_json::from_str(&go_json).unwrap();
            assert_eq!(plain, go_value, "value graph differs from Go's for {name}");
        }
    }

    /// The one case in the corpus where plain serde and Go disagree by bytes but not by value.
    /// Recorded on its own so the [D-022] hazard is visible at the `Post` level: a `Post` whose
    /// props hold `<` must be marshalled with `go_json_marshal` if the bytes are *stored* or
    /// compared, and may be marshalled with serde if they are merely sent.
    #[test]
    fn plain_serde_differs_from_go_only_by_html_escaping() {
        let o = oracle();
        let case = o
            .get("wire")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|c| s(c, "name") == "props_html_escaped")
            .unwrap();
        let go_json = s(case, "json");
        let post: Post = serde_json::from_str(&go_json).unwrap();

        assert_ne!(serde_json::to_string(&post).unwrap(), go_json);
        assert_eq!(crate::utils::go_json_marshal(&post).unwrap(), go_json);
    }

    #[test]
    fn is_valid_matches_go() {
        let o = oracle();
        for case in o.get("is_valid").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let mut post = post_from(case, "post");
            // Filenames is json:"-", so the oracle records it alongside the marshalled post.
            post.filenames =
                serde_json::from_value(case.get("filenames").unwrap().clone()).unwrap_or_default();
            let max = case.get("max_post_size").unwrap().as_u64().unwrap() as usize;

            let want_id = s(case, "error_id");
            let want_detail = s(case, "detailed");

            match post.is_valid(max) {
                Ok(()) => assert_eq!(want_id, "", "{name}: we accept, Go rejects"),
                Err(err) => {
                    assert_eq!(err.id, want_id, "{name}: error id");
                    assert_eq!(err.detailed_error, want_detail, "{name}: detailed error");
                }
            }
        }
    }

    #[test]
    fn pre_save_matches_go() {
        let o = oracle();
        for case in o.get("pre_save").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let mut post = Post {
                id: s(case, "in_id"),
                create_at: case.get("in_create_at").unwrap().as_i64().unwrap(),
                update_at: case.get("in_update_at").unwrap().as_i64().unwrap(),
                original_id: s(case, "in_original_id"),
                ..Default::default()
            };
            let from_clock = b(case, "create_at_from_clock");
            post.pre_save();

            assert_eq!(
                post.id.len(),
                case.get("out_id_len").unwrap().as_u64().unwrap() as usize,
                "{name}: id length"
            );
            if !from_clock {
                assert_eq!(
                    post.create_at,
                    case.get("out_create_at").unwrap().as_i64().unwrap(),
                    "{name}: create_at"
                );
            }
            assert_eq!(
                post.update_at == post.create_at,
                b(case, "update_at_equals_create_at"),
                "{name}: update_at tracks create_at"
            );
            assert_eq!(post.original_id, s(case, "out_original_id"), "{name}");
            assert_eq!(post.props.is_some(), b(case, "props_non_nil"), "{name}");
            assert_eq!(
                post.file_ids.is_some(),
                b(case, "file_ids_non_nil"),
                "{name}"
            );
        }
    }

    #[test]
    fn pre_commit_matches_go() {
        let o = oracle();
        for case in o.get("pre_commit").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let in_file_ids: Option<Vec<String>> =
                serde_json::from_value(case.get("in_file_ids").unwrap().clone()).unwrap();

            // The oracle records only the fields PreCommit writes, so the rest is a fresh post.
            let mut post = Post {
                file_ids: in_file_ids,
                ..Default::default()
            };
            if name == "props_set_is_kept" {
                post.add_prop("a", serde_json::json!("b"));
            }
            if name == "filenames_set_is_kept" {
                post.filenames = vec!["a.txt".into()];
            }
            post.pre_commit();

            let want_props = case.get("out_props").unwrap();
            let got_props = serde_json::to_value(post.props.as_ref().unwrap()).unwrap();
            assert_eq!(&got_props, want_props, "{name}: props");

            let want_ids: Vec<String> =
                serde_json::from_value(case.get("out_file_ids").unwrap().clone()).unwrap();
            assert_eq!(post.file_ids.unwrap(), want_ids, "{name}: file_ids");

            let want_names: Vec<String> =
                serde_json::from_value(case.get("out_filenames").unwrap().clone()).unwrap();
            assert_eq!(post.filenames, want_names, "{name}: filenames");
        }
    }

    #[test]
    fn the_props_accessors_match_go() {
        let o = oracle();
        for case in o.get("props_accessors").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let in_props: Option<StringInterface> =
                serde_json::from_value(case.get("in_props").unwrap().clone()).unwrap();
            let mut post = Post {
                props: in_props,
                ..Default::default()
            };
            let key = s(case, "key");

            match s(case, "op").as_str() {
                "add" => post.add_prop(key.clone(), case.get("value").unwrap().clone()),
                "del" => post.del_prop(&key),
                "get" => {
                    // Go returns a nil `any` for a missing key AND for a stored null.
                    assert_eq!(
                        post.get_prop(&key).is_none(),
                        b(case, "get_was_nil"),
                        "{name}: get nil-ness"
                    );
                    if !b(case, "get_was_nil") {
                        assert_eq!(
                            post.get_prop(&key).unwrap(),
                            case.get("get_result").unwrap(),
                            "{name}: get value"
                        );
                    }
                }
                other => panic!("unknown op {other}"),
            }

            let want = case.get("out_props").unwrap();
            let got = serde_json::to_value(&post.props).unwrap();
            assert_eq!(&got, want, "{name}: resulting props");
        }
    }

    /// `DelProp` sizes its copy `make(map[string]any, len(o.Props)-1)`, which reads like a
    /// negative-size panic on a nil map. It is not: Go clamps a negative map size hint. Pinned
    /// so nobody re-derives the wrong conclusion from the source.
    #[test]
    fn del_prop_does_not_panic_on_a_nil_map_in_go_either() {
        let o = oracle();
        let d = o.get("del_prop_nil_map").unwrap();
        assert!(!b(d, "nil_map_panicked"));
        assert!(!b(d, "empty_map_panicked"));
        assert!(!b(d, "one_entry_panicked"));
    }

    #[test]
    fn sanitize_props_matches_go() {
        let o = oracle();
        for case in o.get("sanitize_props").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let props: StringInterface =
                serde_json::from_value(case.get("in_props").unwrap().clone()).unwrap();
            let remote_id: Option<String> =
                serde_json::from_value(case.get("remote_id").unwrap().clone()).unwrap();

            let mut post = Post {
                props: Some(props),
                remote_id,
                participants: Some(vec![User {
                    id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    password: "hunter2".into(),
                    mfa_secret: "s3cret".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            post.sanitize_props();

            let want = case.get("out_props").unwrap();
            let got = serde_json::to_value(post.props.as_ref().unwrap()).unwrap();
            assert_eq!(&got, want, "{name}");

            let want_pw: Vec<String> =
                serde_json::from_value(case.get("out_participant_passwords").unwrap().clone())
                    .unwrap();
            let got_pw: Vec<String> = post
                .participants
                .unwrap()
                .iter()
                .map(|u| u.password.clone())
                .collect();
            assert_eq!(got_pw, want_pw, "{name}: participants sanitized");
        }
    }

    #[test]
    fn preserve_identity_props_matches_go() {
        let o = oracle();
        for case in o
            .get("preserve_identity_props")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let name = s(case, "name");
            let old_props: Option<StringInterface> =
                serde_json::from_value(case.get("old_props").unwrap().clone()).unwrap();
            let new_props: Option<StringInterface> =
                serde_json::from_value(case.get("new_props").unwrap().clone()).unwrap();

            let mut post = Post {
                props: new_props,
                ..Default::default()
            };
            let old = Post {
                props: old_props,
                ..Default::default()
            };
            post.preserve_identity_props_from(&old);

            let want = case.get("out_props").unwrap();
            let got = serde_json::to_value(&post.props).unwrap();
            assert_eq!(&got, want, "{name}");
        }
    }

    #[test]
    fn sanitize_input_matches_go() {
        let o = oracle();
        for case in o.get("sanitize_input").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let mut post = match name.as_str() {
                "delete_at_and_remote_id_cleared" => Post {
                    delete_at: 12345,
                    remote_id: Some("cluster-a".into()),
                    ..Default::default()
                },
                "metadata_nil_stays_nil" => Post {
                    delete_at: 1,
                    ..Default::default()
                },
                "embeds_cleared" => Post {
                    metadata: Some(PostMetadata {
                        embeds: vec![crate::post_embed::PostEmbed {
                            type_: crate::post_embed::POST_EMBED_LINK.to_string(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                _ => Post::default(),
            };
            post.sanitize_input();

            assert_eq!(
                post.delete_at,
                case.get("out_delete_at").unwrap().as_i64().unwrap(),
                "{name}"
            );
            let want_remote: Option<String> =
                serde_json::from_value(case.get("out_remote_id").unwrap().clone()).unwrap();
            assert_eq!(post.remote_id, want_remote, "{name}");
            assert_eq!(
                post.metadata.is_none(),
                b(case, "out_metadata_nil"),
                "{name}"
            );
            // Go's `Embeds = nil` and our `Vec::new()` are indistinguishable on the wire —
            // every PostMetadata collection carries omitempty, which drops both.
            let embeds_empty = post.metadata.as_ref().is_none_or(|m| m.embeds.is_empty());
            assert_eq!(embeds_empty, b(case, "out_embeds_nil"), "{name}");
        }
    }

    /// The returned order is `reservedProps`' declaration order, never the map's — pinned with
    /// a case whose insertion order is the exact reverse.
    #[test]
    fn contains_integrations_reserved_props_matches_go() {
        let o = oracle();
        for case in o.get("reserved_props").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let props: Option<StringInterface> =
                serde_json::from_value(case.get("props").unwrap().clone()).unwrap();

            let want: Vec<String> =
                serde_json::from_value(case.get("found").unwrap().clone()).unwrap();
            let post = Post {
                props: props.clone(),
                ..Default::default()
            };
            assert_eq!(post.contains_integrations_reserved_props(), want, "{name}");

            let want_patch: Vec<String> =
                serde_json::from_value(case.get("patch_found").unwrap().clone()).unwrap();
            let patch = PostPatch {
                props,
                ..Default::default()
            };
            assert_eq!(
                patch.contains_integrations_reserved_props(),
                want_patch,
                "{name}: patch"
            );
        }
    }

    #[test]
    fn the_notification_predicates_match_go() {
        let o = oracle();
        for case in o.get("notification_props").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let props: Option<StringInterface> =
                serde_json::from_value(case.get("props").unwrap().clone()).unwrap();
            let post = Post {
                props,
                ..Default::default()
            };

            assert_eq!(
                post.has_force_notification(),
                b(case, "has_force"),
                "{name}: force"
            );
            assert_eq!(
                post.has_silent_notification(),
                b(case, "has_silent"),
                "{name}: silent"
            );
            assert_eq!(
                post.is_notification_suppressed(),
                b(case, "suppressed"),
                "{name}: suppressed"
            );
            assert_eq!(
                post.excludes_from_channel_message_count(),
                b(case, "excludes_count"),
                "{name}: excludes count"
            );
            assert_eq!(
                post.has_unsafe_links(),
                b(case, "has_unsafe_links"),
                "{name}: unsafe links"
            );
        }
    }

    /// The asymmetry, called out on its own so it cannot be lost in the loop above: a
    /// `force_notification` of the *string* `"false"` forces a notification.
    #[test]
    fn a_string_false_force_notification_is_truthy_in_go() {
        let o = oracle();
        let case = o
            .get("notification_props")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|c| s(c, "name") == "force_string_false_silent_bool_true")
            .unwrap();
        assert!(b(case, "has_force"));
        assert!(b(case, "has_silent"));
        assert!(!b(case, "suppressed"));
    }

    #[test]
    fn the_type_predicates_match_go() {
        let o = oracle();
        for case in o.get("type_predicates").unwrap().as_array().unwrap() {
            let post_type = s(case, "type");
            let post = Post {
                post_type: post_type.clone(),
                ..Default::default()
            };
            assert_eq!(
                post.is_system_message(),
                b(case, "is_system_message"),
                "{post_type}: is_system_message"
            );
            assert_eq!(
                post.is_join_leave_message(),
                b(case, "is_join_leave"),
                "{post_type}: is_join_leave"
            );
            assert_eq!(
                post.is_access_control_team_membership_notification(),
                b(case, "is_acl_membership_notification"),
                "{post_type}: acl"
            );
            assert_eq!(
                post.excludes_from_channel_message_count(),
                b(case, "excludes_count"),
                "{post_type}: excludes_count"
            );
        }
    }

    #[test]
    fn patch_matches_go() {
        let o = oracle();
        for case in o.get("patch").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let patch: PostPatch =
                serde_json::from_value(case.get("patch").unwrap().clone()).unwrap();

            let mut props = StringInterface::new();
            props.insert("old".into(), serde_json::json!("props"));
            let mut post = Post {
                id: "6bdz674pgq767e4jx75w4pf57a".into(),
                message: "  original  ".into(),
                has_reactions: true,
                props: Some(props),
                file_ids: Some(vec![
                    "6bdz674pgq767e4jx75w4pf57a".into(),
                    "qr6kf7ztp7yifxt4wm5xn51bke".into(),
                ]),
                ..Default::default()
            };
            post.patch(&patch);

            let want = case.get("out").unwrap();
            let got = serde_json::to_value(&post).unwrap();
            assert_eq!(&got, want, "{name}");
        }
    }

    #[test]
    fn patch_is_empty_matches_go() {
        let o = oracle();
        for case in o.get("patch_is_empty").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let patch: PostPatch =
                serde_json::from_value(case.get("patch").unwrap().clone()).unwrap();
            assert_eq!(patch.is_empty(), b(case, "is_empty"), "{name}");
        }
    }

    #[test]
    fn etag_matches_go() {
        let o = oracle();
        for case in o.get("etag").unwrap().as_array().unwrap() {
            let post = Post {
                id: s(case, "id"),
                update_at: case.get("update_at").unwrap().as_i64().unwrap(),
                ..Default::default()
            };
            assert_eq!(post.etag(), s(case, "etag"), "{}", s(case, "name"));
        }
    }

    #[test]
    fn find_at_channel_mention_matches_go() {
        let o = oracle();
        for case in o
            .get("find_at_channel_mention")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let input = s(case, "input");
            let got = find_at_channel_mention(&input);
            assert_eq!(got.is_some(), b(case, "found"), "{input:?}: found");
            assert_eq!(
                got.unwrap_or_default(),
                s(case, "mention"),
                "{input:?}: mention"
            );
        }
    }

    #[test]
    fn disable_mention_highlights_matches_go() {
        let o = oracle();
        for case in o
            .get("disable_mention_highlights")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let name = s(case, "name");
            let props: Option<StringInterface> =
                serde_json::from_value(case.get("in_props").unwrap().clone()).unwrap();
            let message = s(case, "message");

            let mut post = Post {
                message: message.clone(),
                props,
                ..Default::default()
            };
            let mention = post.disable_mention_highlights();
            assert_eq!(
                mention.unwrap_or_default(),
                s(case, "mention"),
                "{name}: mention"
            );
            assert_eq!(
                serde_json::to_value(&post.props).unwrap(),
                *case.get("out_props").unwrap(),
                "{name}: post props"
            );

            let mut patch = PostPatch {
                message: Some(message),
                ..Default::default()
            };
            patch.disable_mention_highlights();
            assert_eq!(
                serde_json::to_value(&patch.props).unwrap(),
                *case.get("patch_props").unwrap(),
                "{name}: patch props"
            );
        }
    }

    #[test]
    fn is_from_oauth_bot_matches_go() {
        let o = oracle();
        for case in o.get("is_from_oauth_bot").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let props: Option<StringInterface> =
                serde_json::from_value(case.get("props").unwrap().clone()).unwrap();
            let post = Post {
                props,
                ..Default::default()
            };
            assert_eq!(
                post.is_from_oauth_bot(),
                b(case, "is_from_oauth_bot"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_priority_accessors_match_go() {
        let o = oracle();
        for case in o.get("priority_accessors").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let post = post_from(case, "post");

            assert_eq!(
                post.get_priority().is_none(),
                b(case, "priority_nil"),
                "{name}"
            );
            assert_eq!(
                serde_json::to_value(post.get_priority()).unwrap(),
                *case.get("priority").unwrap(),
                "{name}: priority"
            );
            let want_persistent: Option<bool> =
                serde_json::from_value(case.get("persistent_notifications").unwrap().clone())
                    .unwrap();
            assert_eq!(
                post.get_persistent_notification(),
                want_persistent,
                "{name}"
            );
            let want_ack: Option<bool> =
                serde_json::from_value(case.get("requested_ack").unwrap().clone()).unwrap();
            assert_eq!(post.get_requested_ack(), want_ack, "{name}");
            assert_eq!(post.is_urgent(), b(case, "is_urgent"), "{name}: urgent");
            assert_eq!(
                post.get_previewed_post_prop(),
                s(case, "previewed_post_prop"),
                "{name}: previewed post"
            );
        }
    }

    #[test]
    fn the_misc_accessors_match_go() {
        let o = oracle();
        for case in o.get("misc_accessors").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let post = match name.as_str() {
                "zero" => Post::default(),
                "remote_id_nil" => Post {
                    id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    ..Default::default()
                },
                "remote_id_empty" => Post {
                    id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    remote_id: Some(String::new()),
                    ..Default::default()
                },
                _ => Post {
                    id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    remote_id: Some("cluster-a".into()),
                    ..Default::default()
                },
            };
            assert_eq!(post.is_remote(), b(case, "is_remote"), "{name}");
            assert_eq!(post.get_remote_id(), s(case, "remote_id"), "{name}");
            assert_eq!(
                post.to_nil_if_invalid().is_none(),
                b(case, "to_nil_if_invalid"),
                "{name}"
            );

            // CleanPost leaves delete_at alone — the same corpus records it every iteration.
            let mut cleanable = Post {
                id: "6bdz674pgq767e4jx75w4pf57a".into(),
                create_at: 1,
                update_at: 2,
                edit_at: 3,
                delete_at: 4,
                message: "keep".into(),
                ..Default::default()
            };
            cleanable.clean_post();
            assert_eq!(
                serde_json::to_value(&cleanable).unwrap(),
                *case.get("clean_post").unwrap(),
                "{name}: clean_post"
            );
        }
    }

    /// Go's `ShallowCopy` deep-copies exactly one field and aliases every other reference.
    /// Rust's `Clone` owns its values, so ours is genuinely independent — [D-036].
    #[test]
    fn clone_diverges_from_gos_aliasing_by_design() {
        let o = oracle();
        let c = o.get("clone").unwrap();
        assert!(b(c, "props_aliased"));
        assert!(b(c, "file_ids_aliased"));
        assert!(b(c, "metadata_aliased"));
        assert!(b(c, "is_following_deep_copied"));
        assert!(b(c, "shallow_copy_nil_dst_error"));

        // Ours: mutating the clone leaves the original untouched everywhere.
        let mut original = Post {
            id: "6bdz674pgq767e4jx75w4pf57a".into(),
            file_ids: Some(vec!["qr6kf7ztp7yifxt4wm5xn51bke".into()]),
            metadata: Some(PostMetadata {
                redacted_file_count: 3,
                ..Default::default()
            }),
            is_following: Some(true),
            ..Default::default()
        };
        original.add_prop("a", serde_json::json!("b"));

        let mut clone = original.clone();
        clone.add_prop("a", serde_json::json!("mutated"));
        clone.file_ids.as_mut().unwrap()[0] = "mutated".into();
        clone.metadata.as_mut().unwrap().redacted_file_count = 99;
        clone.is_following = Some(false);

        assert_eq!(original.get_prop("a"), Some(&serde_json::json!("b")));
        assert_eq!(
            original.file_ids.as_ref().unwrap()[0],
            "qr6kf7ztp7yifxt4wm5xn51bke"
        );
        assert_eq!(original.metadata.as_ref().unwrap().redacted_file_count, 3);
        assert_eq!(original.is_following, Some(true));
    }

    /// `IsValid`'s three length caps measure Go's `encoding/json`, not serde_json's. The
    /// escaping differs on five characters, and a nil collection is `"null"` rather than `[]`
    /// or `{}` — four runes that count against the cap.
    #[test]
    fn the_marshallers_is_valid_measures_match_go() {
        let o = oracle();
        for case in o.get("array_to_json").unwrap().as_array().unwrap() {
            let input: Option<Vec<String>> =
                serde_json::from_value(case.get("input").unwrap().clone()).unwrap();
            let got = crate::utils::array_to_json(input.as_deref());
            let want = s(case, "json");
            assert_eq!(got, want, "array_to_json({input:?})");
            assert_eq!(
                got.chars().count() as i64,
                case.get("rune_count").unwrap().as_i64().unwrap(),
                "rune count for {input:?}"
            );
        }

        for case in o
            .get("string_interface_to_json")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let input: Option<StringInterface> =
                serde_json::from_value(case.get("input").unwrap().clone()).unwrap();
            let got = crate::utils::string_interface_to_json(input.as_ref());
            let want = s(case, "json");
            assert_eq!(got, want, "string_interface_to_json({input:?})");
            assert_eq!(
                got.chars().count() as i64,
                case.get("rune_count").unwrap().as_i64().unwrap(),
                "rune count for {input:?}"
            );
        }
    }

    #[test]
    fn should_index_matches_go() {
        use crate::file_info::{BOOKMARK_FILE_OWNER, FileInfo};
        let o = oracle();
        for case in o.get("should_index").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let want = b(case, "should_index");

            // Go's receiver is nilable; Rust carries that case as an Option at the call site.
            let file: Option<FileForIndexing> = match name.as_str() {
                "nil" => None,
                "zero" => Some(FileForIndexing::default()),
                "deleted" => Some(FileForIndexing {
                    file_info: FileInfo {
                        delete_at: 1,
                        post_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                "has_post_id" => Some(FileForIndexing {
                    file_info: FileInfo {
                        post_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                "bookmark_owner" => Some(FileForIndexing {
                    file_info: FileInfo {
                        creator_id: BOOKMARK_FILE_OWNER.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                "other_creator_no_post" => Some(FileForIndexing {
                    file_info: FileInfo {
                        creator_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                _ => Some(FileForIndexing {
                    file_info: FileInfo {
                        delete_at: 1,
                        creator_id: BOOKMARK_FILE_OWNER.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
            };

            assert_eq!(file.is_some_and(|f| f.should_index()), want, "{name}");
        }
    }

    #[test]
    fn sync_cursor_is_empty_matches_go() {
        let o = oracle();
        for case in o.get("sync_cursor_is_empty").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let cursor = match name.as_str() {
                "update_at" => GetPostsSinceForSyncCursor {
                    last_post_update_at: 1,
                    ..Default::default()
                },
                "update_id" => GetPostsSinceForSyncCursor {
                    last_post_update_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    ..Default::default()
                },
                "create_at" => GetPostsSinceForSyncCursor {
                    last_post_create_at: 1,
                    ..Default::default()
                },
                "create_id" => GetPostsSinceForSyncCursor {
                    last_post_create_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    ..Default::default()
                },
                _ => GetPostsSinceForSyncCursor::default(),
            };
            assert_eq!(cursor.is_empty(), b(case, "is_empty"), "{name}");
        }
    }
}

/// Parity for post.go chunk 2 against `fixtures/behaviour_post_attachments.json`.
#[cfg(test)]
mod attachments_go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_attachments.json"
        ))
        .unwrap()
    }

    fn cases(section: &str) -> Vec<Value> {
        oracle().get(section).unwrap().as_array().unwrap().to_vec()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_str(&s(v, key)).unwrap()
    }

    /// Go's `[]string` is nil when nothing was appended, and marshals as `null`; ours is an
    /// empty `Vec`.
    fn go_strings(v: &Value, key: &str) -> Vec<String> {
        match v.get(key).unwrap() {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect(),
            other => panic!("not a string list: {other}"),
        }
    }

    /// The two corpus cases we cannot reproduce, each for a reason asserted separately below.
    const DIVERGENT: [&str; 3] = [
        "action_option_null",
        "action_option_null_then_real",
        "case_insensitive_keys",
    ];

    #[test]
    fn attachments_matches_go() {
        for case in cases("attachments") {
            let name = s(&case, "name");
            if DIVERGENT.contains(&name.as_str()) {
                continue;
            }

            let post = post_from(&case, "post");
            let ours = serde_json::to_value(post.attachments()).unwrap();

            // Go marshals a nil slice as `null`; ours is `[]`.
            let theirs: Value = match serde_json::from_str(&s(&case, "attachments")).unwrap() {
                Value::Null => Value::Array(Vec::new()),
                other => other,
            };

            assert_eq!(ours, theirs, "{name}");
            assert_eq!(
                post.attachments().len() as u64,
                case.get("count").unwrap().as_u64().unwrap(),
                "{name} count"
            );
        }
    }

    /// A bare `null` element survives Go's marshal/unmarshal round trip untouched, so it
    /// contributes a **zero attachment** rather than being dropped. The source reads like it
    /// would be dropped.
    #[test]
    fn a_null_element_becomes_a_zero_attachment() {
        let case = cases("attachments")
            .into_iter()
            .find(|c| s(c, "name") == "element_null")
            .unwrap();

        assert_eq!(case.get("count").unwrap().as_u64().unwrap(), 1);
        let got = post_from(&case, "post").attachments();
        assert_eq!(got, vec![MessageAttachment::default()]);
    }

    /// [D-033]: Go keeps a nil option inside an action, so the attachment survives with
    /// `"options":[null]`. `Vec<PostActionOptions>` cannot hold that, so the decode fails and we
    /// drop the whole attachment — one attachment fewer than Go, not merely one option fewer.
    #[test]
    fn a_nil_action_option_drops_the_attachment_where_go_keeps_it() {
        for name in ["action_option_null", "action_option_null_then_real"] {
            let case = cases("attachments")
                .into_iter()
                .find(|c| s(c, "name") == name)
                .unwrap();

            assert_eq!(case.get("count").unwrap().as_u64().unwrap(), 1, "{name}");
            assert!(
                s(&case, "attachments").contains("\"options\":[null"),
                "{name}: Go kept the nil option"
            );
            assert!(post_from(&case, "post").attachments().is_empty(), "{name}");
        }
    }

    /// [D-040]: `encoding/json` matches struct fields case-insensitively; serde does not. Go
    /// reads `{"Title":"t","TEXT":"x"}` as a populated attachment, we read it as an empty one.
    #[test]
    fn case_insensitive_keys_are_go_only() {
        let case = cases("attachments")
            .into_iter()
            .find(|c| s(c, "name") == "case_insensitive_keys")
            .unwrap();

        assert!(s(&case, "attachments").contains("\"title\":\"t\""));
        assert!(s(&case, "attachments").contains("\"text\":\"x\""));

        let ours = post_from(&case, "post").attachments();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].title, "");
        assert_eq!(ours[0].text, "");
    }

    #[test]
    fn attachments_equal_matches_go() {
        for case in cases("attachments_equal") {
            let name = s(&case, "name");
            let a = post_from(&case, "a");
            let b_post = post_from(&case, "b");
            let ours = a.attachments_equal(&b_post);

            if b(&case, "panicked") {
                // [D-039]: Go crashes comparing a field whose value is nil, which is any field
                // with no `value` key. Ours answers instead; the answer is asserted in
                // `equals_answers_where_go_panics` so the divergence cannot rot.
                continue;
            }
            assert_eq!(ours, b(&case, "equal"), "{name}");
        }
    }

    /// The two cases where Go panics rather than answering, with what we answer instead.
    #[test]
    fn equals_answers_where_go_panics() {
        let panicking: Vec<String> = cases("attachments_equal")
            .iter()
            .filter(|c| b(c, "panicked"))
            .map(|c| s(c, "name"))
            .collect();
        assert_eq!(
            panicking,
            ["field_without_value", "field_value_explicit_null"]
        );

        for (name, expected) in [
            ("field_without_value", true),
            ("field_value_explicit_null", false),
        ] {
            let case = cases("attachments_equal")
                .into_iter()
                .find(|c| s(c, "name") == name)
                .unwrap();
            let ours = post_from(&case, "a").attachments_equal(&post_from(&case, "b"));
            assert_eq!(ours, expected, "{name}");
        }
    }

    /// A malformed attachment is dropped rather than compared, so a post carrying only that
    /// attachment is equal to a post carrying none.
    #[test]
    fn a_dropped_attachment_is_absent_not_unequal() {
        let case = cases("attachments_equal")
            .into_iter()
            .find(|c| s(c, "name") == "malformed_against_absent")
            .unwrap();
        assert!(b(&case, "equal"));
        assert!(post_from(&case, "a").attachments_equal(&post_from(&case, "b")));
    }

    #[test]
    fn all_strings_matches_go() {
        for case in cases("all_strings") {
            let name = s(&case, "name");
            let post = post_from(&case, "post");
            assert_eq!(
                post.all_strings(AllStringsOptions {
                    omit_interactive_blocks: true
                }),
                go_strings(&case, "omitting"),
                "{name}"
            );
            assert_eq!(
                post.all_strings(AllStringsOptions {
                    omit_interactive_blocks: false
                }),
                go_strings(&case, "full"),
                "{name}"
            );
        }
    }

    /// The four corpus cases carrying an interactive payload are exactly the ones where the two
    /// option values differ. That difference was [D-041] — the unported half of `AllStrings` —
    /// until `post_interactive_blocks.go` landed; the assertion now runs in the other direction,
    /// so a regression in either walker fails here as well as in that module.
    #[test]
    fn the_interactive_half_is_the_only_difference_between_the_options() {
        let differing: Vec<String> = cases("all_strings")
            .iter()
            .filter(|c| b(c, "differs"))
            .map(|c| s(c, "name"))
            .collect();
        assert_eq!(
            differing,
            [
                "mm_blocks_present",
                "block_kit_present",
                "adaptive_cards_present",
                "attachments_and_mm_blocks",
            ]
        );

        for case in cases("all_strings").iter().filter(|c| b(c, "differs")) {
            let name = s(case, "name");
            let post = post_from(case, "post");
            let omitting = post.all_strings(AllStringsOptions {
                omit_interactive_blocks: true,
            });
            let full = post.all_strings(AllStringsOptions {
                omit_interactive_blocks: false,
            });

            assert_eq!(omitting, go_strings(case, "omitting"), "{name}");
            assert_eq!(full, go_strings(case, "full"), "{name}");
            assert!(omitting.len() < full.len(), "{name}");
            assert_eq!(
                full[..omitting.len()],
                omitting[..],
                "{name}: the interactive strings are appended last"
            );
        }
    }

    /// Trimming is asymmetric: a string value keeps its padding, a rendered one loses it.
    #[test]
    fn a_string_field_value_keeps_its_padding_and_a_rendered_one_does_not() {
        let padded = cases("all_strings")
            .into_iter()
            .find(|c| s(c, "name") == "field_string_value_padded")
            .unwrap();
        assert_eq!(
            post_from(&padded, "post").all_strings(AllStringsOptions::default()),
            ["ft", "  fv  "]
        );

        let rendered = cases("all_strings")
            .into_iter()
            .find(|c| s(c, "name") == "field_value_big_float")
            .unwrap();
        assert_eq!(
            post_from(&rendered, "post").all_strings(AllStringsOptions::default()),
            ["ft", "1.23456789e+08"]
        );
    }

    /// Go's `unicode.IsSpace` and Rust's `char::is_whitespace` agree on every probe, including
    /// the two that look like spaces and are not.
    #[test]
    fn the_whitespace_test_matches_go() {
        for (name, kept) in [
            ("message_spaces", false),
            ("message_tab_newline", false),
            ("message_nbsp", false),
            ("message_ideographic_space", false),
            ("message_ogham_space_mark", false),
            ("message_next_line", false),
            ("message_zero_width_space", true),
            ("message_mongolian_vowel_separator", true),
        ] {
            let case = cases("all_strings")
                .into_iter()
                .find(|c| s(c, "name") == name)
                .unwrap();
            assert_eq!(
                !go_strings(&case, "omitting").is_empty(),
                kept,
                "{name}: the oracle disagrees with the expectation in this test"
            );
            assert_eq!(
                !post_from(&case, "post")
                    .all_strings(AllStringsOptions::default())
                    .is_empty(),
                kept,
                "{name}"
            );
        }
    }
}

/// Oracle-driven tests for `ToJSON`, `EncodeJSON` and `DelProp`, all three of which were
/// measured alongside integration_action.go chunk 3 in `fixtures/behaviour_post_actions.json`.
///
/// The two serialisers differ in exactly one way that matters and it is easy to get backwards:
/// `ToJSON` strips a **copy** and `EncodeJSON` strips the receiver. Each test asserts the
/// receiver's state after the call as well as the output.
#[cfg(test)]
mod serialise_go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_actions.json"
        ))
        .unwrap()
    }

    fn section(o: &Value, key: &str) -> Vec<Value> {
        o.get(key).unwrap().as_array().unwrap().clone()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_str(&s(v, key)).unwrap()
    }

    /// Byte-for-byte unless the answer carries a rewritten attachment list, where Go's struct
    /// field order and our sorted `serde_json::Map` disagree — see [D-048], pinned in
    /// `integration_action::post_actions_go_parity`.
    fn assert_json_matches_go(name: &str, ours: &str, want: &str) {
        if want.contains("\"attachments\":[") {
            let got: Value = serde_json::from_str(ours).unwrap();
            let want: Value = serde_json::from_str(want).unwrap();
            assert_eq!(got, want, "{name}");
        } else {
            assert_eq!(ours, want, "{name}");
        }
    }

    #[test]
    fn to_json_matches_go() {
        for case in section(&oracle(), "to_json") {
            let name = s(&case, "name");
            assert_eq!(s(&case, "err"), "", "{name}: Go reported an error");

            let post = post_from(&case, "post");
            let got = post.to_json().unwrap();
            assert_json_matches_go(&name, &got, &s(&case, "out"));

            // The receiver keeps its integrations: Go strips a clone.
            assert_json_matches_go(
                &name,
                &go_json_marshal(&post).unwrap(),
                &s(&case, "receiver_after"),
            );
        }
    }

    #[test]
    fn encode_json_matches_go() {
        for case in section(&oracle(), "encode_json") {
            let name = s(&case, "name");
            assert_eq!(s(&case, "err"), "", "{name}: Go reported an error");

            let mut post = post_from(&case, "post");
            let mut buf = Vec::new();
            post.encode_json(&mut buf).unwrap();
            let got = String::from_utf8(buf).unwrap();

            let want = s(&case, "out");
            assert!(
                want.ends_with('\n'),
                "{name}: Go's encoder writes a newline"
            );
            assert_json_matches_go(
                &name,
                got.trim_end_matches('\n'),
                want.trim_end_matches('\n'),
            );
            assert!(got.ends_with('\n'), "{name}: we must write it too");

            // The receiver is stripped in place: Go does not clone here.
            assert_json_matches_go(
                &name,
                &go_json_marshal(&post).unwrap(),
                &s(&case, "receiver_after"),
            );
        }
    }

    /// The pair that is easy to swap. `to_json` leaves an integration on the receiver where
    /// `encode_json` destroys it, and a caller that picked the wrong one either leaks private
    /// plugin data or loses it.
    #[test]
    fn to_json_clones_and_encode_json_does_not() {
        let raw = r#"{"props":{"attachments":[{"actions":[{"id":"a","integration":{"url":"https://x.example.com"}}]}]}}"#;

        let mut post: Post = serde_json::from_str(raw).unwrap();
        post.to_json().unwrap();
        assert!(post.attachments()[0].actions[0].integration.is_some());

        post.encode_json(&mut Vec::new()).unwrap();
        assert!(post.attachments()[0].actions[0].integration.is_none());
    }

    #[test]
    fn del_prop_matches_go() {
        for case in section(&oracle(), "del_prop") {
            let name = s(&case, "name");
            let mut post = post_from(&case, "post");
            post.del_prop(&s(&case, "key"));
            assert_eq!(go_json_marshal(&post).unwrap(), s(&case, "out"), "{name}");
        }
    }
}
