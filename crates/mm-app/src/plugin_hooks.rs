//! Go's plugin hook call sites on the write paths this server serves, and the two dispatchers
//! behind them (`app/channels.go:341` `RunMultiHook*`, `app/guarded_hooks.go`,
//! `app/context.go:41` `pluginContext`).
//!
//! # Only under the Rust plugin host
//!
//! Every entry point here returns immediately unless [`crate::plugins::PluginHost::hosted`] —
//! `MMRS_PLUGIN_HOST=rust` — and [`crate::App::plugins_environment`] is `Some`, which is Go's
//! `GetPluginsEnvironment() != nil` (`PluginSettings.Enable` plus a started environment). Under
//! the default Go host the plugins live in the other process and firing them from here would be
//! two HA nodes with no cluster bus (docs/PLUGIN_PLAN.md, D6), so nothing fires and the
//! behaviour is what it was before this module existed.
//!
//! # The two shapes
//!
//! - **`*HasBeen*`** hooks are notifications. Go runs each in `a.Srv().Go(...)`, a tracked
//!   goroutine, over a clone of the value taken on the calling goroutine; the closure always
//!   answers `true`, so every plugin is called and nothing short-circuits. Here that is
//!   [`spawn_multi_hook`] with the wire value built before the spawn, for the same reason: the
//!   caller keeps mutating the post.
//! - **`MessageWillBe*`** hooks can reject or replace, and go through `guarded_hooks.go`'s
//!   two-phase dispatcher: phase A fans out to every plugin that is *not* a guard of the
//!   channel, fail-open; phase B calls each guard claimant in `PluginId` order, fail-closed. The
//!   two hooks' rejection contracts differ, and the difference is load-bearing — see
//!   [`App::run_guarded_message_will_be_posted`] and
//!   [`App::run_guarded_message_will_be_updated`].
//!
//! Go has no per-hook timeout anywhere on this path (`environment.go:629` only *observes* the
//! duration for metrics), so neither does this. A plugin that never answers blocks the request,
//! exactly as it does in Go.
//!
//! # Conversion
//!
//! `mm-plugin`'s wire types carry Go field names and everything gob sends, including the
//! `json:"-"` fields `mm-model` skips (docs/PLUGIN_PLAN.md, D5). The conversion is therefore
//! explicit, here, in the host — [`post_to_wire`] and [`post_from_wire`] — rather than a serde
//! derive on a JSON-shaped struct.

use std::sync::Arc;

use mm_model::channel::{Channel, ChannelBannerInfo};
use mm_model::channel_member::ChannelMember;
use mm_model::draft::Draft;
use mm_model::file_info::FileInfo;
use mm_model::post::{POST_TYPE_BURN_ON_READ, Post};
use mm_model::post_list::PostMap;
use mm_model::post_metadata::PostMetadata;
use mm_model::preference::Preferences;
use mm_model::reaction::Reaction;
use mm_model::scheduled_post::ScheduledPost;
use mm_model::team_member::TeamMember;
use mm_model::user::User;
use mm_model::utils::AppError;
use mm_plugin::rpc::{HooksClient, hook_id, json_to_interface};
use mm_plugin::wire::plugin as wire_plugin;
use mm_plugin::wire::{interface_to_json, model as wire_model};

use crate::App;
use crate::plugins::PluginsEnvironment;

/// `plugin.DismissPostError` (public/plugin/hooks.go:82): the one rejection reason that is not
/// prefixed, because the client treats it as "quietly drop the pending post".
pub const DISMISS_POST_ERROR: &str = "plugin.message_will_be_posted.dismiss_post";

/// The prefix `guarded_hooks.go` puts in front of every other rejection reason. It is the error
/// **id**, not a translation key and not a parameter, so it reaches the client verbatim.
const REJECTED_PREFIX: &str = "Post rejected by plugin. ";

// -------------------------------------------------------------------------------------------
// plugin.Context
// -------------------------------------------------------------------------------------------

/// Port of `pluginContext` (app/context.go:41) — the six fields of `plugin.Context`, carried
/// from the HTTP request to the hook.
///
/// Go builds it from `request.CTX`, which `web.Handler.ServeHTTP` fills from the request
/// (`web/handlers.go:191-205`). There is no request context in this tree, so `mm-api` builds one
/// of these per request (`mm_api::plugin_context`) and hands it to the app function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookContext {
    /// `model.NewId()`, minted per request by the handler — never the client's.
    pub request_id: String,
    /// `rctx.Session().Id`.
    pub session_id: String,
    /// `utils.GetIPAddress`.
    pub ip_address: String,
    /// The `Accept-Language` header, verbatim.
    pub accept_language: String,
    /// The `User-Agent` header, verbatim.
    pub user_agent: String,
    /// The `Connection-Id` header, which is empty unless the client sent one.
    pub connection_id: String,
}

impl HookContext {
    pub(crate) fn to_wire(&self) -> wire_plugin::Context {
        wire_plugin::Context {
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            ip_address: self.ip_address.clone(),
            accept_language: self.accept_language.clone(),
            user_agent: self.user_agent.clone(),
            connection_id: self.connection_id.clone(),
        }
    }

    fn boxed_wire(&self) -> Option<Box<wire_plugin::Context>> {
        Some(Box::new(self.to_wire()))
    }
}

// -------------------------------------------------------------------------------------------
// model <-> wire
// -------------------------------------------------------------------------------------------

/// `map[string]any` as gob sends it: `client_rpc.go`'s `init()` registers `[]any` and
/// `map[string]any`, so a nested document encodes; every number is a `float64`, because that is
/// what Go's own `json.Unmarshal` of the request body left in the map.
pub(crate) fn props_to_wire(
    props: Option<&mm_model::utils::StringInterface>,
) -> wire_model::StringInterface {
    props
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), json_to_interface(v)))
        .collect()
}

/// The reverse. Go's numbers come back as `float64`, and `go_normalize_json_numbers` writes an
/// integral one the way Go's `json.Marshal` does — without it a prop that went out as `5` would
/// be stored as `5.0`.
pub(crate) fn props_from_wire(
    props: &wire_model::StringInterface,
) -> mm_model::utils::StringInterface {
    let mut out = mm_model::utils::StringInterface::new();
    for (key, value) in props {
        let mut json = match value {
            Some(i) => interface_to_json(i).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        mm_model::utils::go_normalize_json_numbers(&mut json);
        out.insert(key.clone(), json);
    }
    out
}

/// A `model.StringMap` as gob sends it.
///
/// `mm-model` models Go's nil map as `None`, and gob omits an empty map as readily as a nil one,
/// so the two are indistinguishable once they cross — which is why [`channel_member_from_wire`]
/// and [`team_member_from_wire`] take the caller's value back when what returned is empty.
pub(crate) fn string_map_to_wire(
    map: Option<&mm_model::utils::StringMap>,
) -> wire_model::StringMap {
    map.into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The reverse: a gob `StringMap` (a `HashMap`) into `mm-model`'s ordered one.
fn string_map_from_wire(map: &wire_model::StringMap) -> mm_model::utils::StringMap {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// A user as gob sends it, all 34 fields of `model.User` (user.go:86).
///
/// **Nothing is sanitised here, because Go sanitises nothing at these call sites.** `Password`,
/// `AuthData` and `MfaSecret` are ordinary gob fields — they carry `json:"-"`-ish JSON tags but
/// gob matches by Go field name (docs/PLUGIN_PLAN.md, §2) — and `AddChannelMember`,
/// `removeUserFromChannel`, `JoinUserToTeam` and `postProcessTeamMemberLeave` all hand the hook
/// `a.GetUser(...)`, which is the raw store row; so do the two login hooks. The two users that
/// *are* sanitised are `UserHasBeenCreated`'s (`userService.createUser`) and
/// `UserHasBeenDeactivated`'s (`SqlUserStore.Update`), both before the hook and above this
/// conversion.
pub fn user_to_wire(user: &User) -> wire_model::User {
    wire_model::User {
        id: user.id.clone(),
        create_at: user.create_at,
        update_at: user.update_at,
        delete_at: user.delete_at,
        username: user.username.clone(),
        password: user.password.clone(),
        auth_data: user.auth_data.clone(),
        auth_service: user.auth_service.clone(),
        email: user.email.clone(),
        email_verified: user.email_verified,
        nickname: user.nickname.clone(),
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        position: user.position.clone(),
        roles: user.roles.clone(),
        allow_marketing: user.allow_marketing,
        props: string_map_to_wire(user.props.as_ref()),
        notify_props: string_map_to_wire(user.notify_props.as_ref()),
        last_password_update: user.last_password_update,
        last_picture_update: user.last_picture_update,
        failed_attempts: user.failed_attempts,
        locale: user.locale.clone(),
        timezone: string_map_to_wire(user.timezone.as_ref()),
        mfa_active: user.mfa_active,
        mfa_secret: user.mfa_secret.clone(),
        remote_id: user.remote_id.clone(),
        last_activity_at: user.last_activity_at,
        is_bot: user.is_bot,
        bot_description: user.bot_description.clone(),
        bot_last_icon_update: user.bot_last_icon_update,
        terms_of_service_id: user.terms_of_service_id.clone(),
        terms_of_service_create_at: user.terms_of_service_create_at,
        disable_welcome_email: user.disable_welcome_email,
        last_login: user.last_login,
        mfa_used_timestamps: user.mfa_used_timestamps.clone().unwrap_or_default(),
    }
}

/// The reverse of [`user_to_wire`], for the one place a user comes **back** from a plugin: a
/// post's `Participants`, when a `MessagesWillBeConsumed` answer carries the whole post
/// ([`post_from_wire_whole`]). A nil and an empty map are one thing to gob, so the three maps
/// and the timestamp list are `None` when what came back is empty.
pub fn user_from_wire(wire: &wire_model::User) -> User {
    let map = |m: &wire_model::StringMap| (!m.is_empty()).then(|| string_map_from_wire(m));
    User {
        id: wire.id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        username: wire.username.clone(),
        password: wire.password.clone(),
        auth_data: wire.auth_data.clone(),
        auth_service: wire.auth_service.clone(),
        email: wire.email.clone(),
        email_verified: wire.email_verified,
        nickname: wire.nickname.clone(),
        first_name: wire.first_name.clone(),
        last_name: wire.last_name.clone(),
        position: wire.position.clone(),
        roles: wire.roles.clone(),
        allow_marketing: wire.allow_marketing,
        props: map(&wire.props),
        notify_props: map(&wire.notify_props),
        last_password_update: wire.last_password_update,
        last_picture_update: wire.last_picture_update,
        failed_attempts: wire.failed_attempts,
        locale: wire.locale.clone(),
        timezone: map(&wire.timezone),
        mfa_active: wire.mfa_active,
        mfa_secret: wire.mfa_secret.clone(),
        remote_id: wire.remote_id.clone(),
        last_activity_at: wire.last_activity_at,
        is_bot: wire.is_bot,
        bot_description: wire.bot_description.clone(),
        bot_last_icon_update: wire.bot_last_icon_update,
        terms_of_service_id: wire.terms_of_service_id.clone(),
        terms_of_service_create_at: wire.terms_of_service_create_at,
        disable_welcome_email: wire.disable_welcome_email,
        last_login: wire.last_login,
        mfa_used_timestamps: (!wire.mfa_used_timestamps.is_empty())
            .then(|| wire.mfa_used_timestamps.clone()),
    }
}

/// A channel member as gob sends it, field for field (`model.ChannelMember`,
/// channel_member.go:22). `NotifyProps` is one of the fields tagged `json:"-"` that gob still
/// carries (docs/PLUGIN_PLAN.md, §2).
pub fn channel_member_to_wire(member: &ChannelMember) -> wire_model::ChannelMember {
    wire_model::ChannelMember {
        channel_id: member.channel_id.clone(),
        user_id: member.user_id.clone(),
        roles: member.roles.clone(),
        last_viewed_at: member.last_viewed_at,
        msg_count: member.msg_count,
        mention_count: member.mention_count,
        mention_count_root: member.mention_count_root,
        urgent_mention_count: member.urgent_mention_count,
        msg_count_root: member.msg_count_root,
        notify_props: string_map_to_wire(member.notify_props.as_ref()),
        last_update_at: member.last_update_at,
        scheme_guest: member.scheme_guest,
        scheme_user: member.scheme_user,
        scheme_admin: member.scheme_admin,
        explicit_roles: member.explicit_roles.clone(),
        auto_translation_disabled: member.auto_translation_disabled,
    }
}

/// A member a `ChannelMemberWillBeAdded` plugin answered with, back into the model.
///
/// `source` is the member the call was made with, for the same reason as
/// [`post_from_wire`]: gob cannot tell an empty map from an absent one, and a member whose
/// `NotifyProps` came back empty has to keep the caller's — an empty one would fail
/// `ChannelMember::is_valid` inside the store, turning every add into a 500.
pub fn channel_member_from_wire(
    wire: &wire_model::ChannelMember,
    source: &ChannelMember,
) -> ChannelMember {
    ChannelMember {
        channel_id: wire.channel_id.clone(),
        user_id: wire.user_id.clone(),
        roles: wire.roles.clone(),
        last_viewed_at: wire.last_viewed_at,
        msg_count: wire.msg_count,
        mention_count: wire.mention_count,
        mention_count_root: wire.mention_count_root,
        urgent_mention_count: wire.urgent_mention_count,
        msg_count_root: wire.msg_count_root,
        notify_props: if wire.notify_props.is_empty() {
            source.notify_props.clone()
        } else {
            Some(string_map_from_wire(&wire.notify_props))
        },
        last_update_at: wire.last_update_at,
        scheme_guest: wire.scheme_guest,
        scheme_user: wire.scheme_user,
        scheme_admin: wire.scheme_admin,
        explicit_roles: wire.explicit_roles.clone(),
        auto_translation_disabled: wire.auto_translation_disabled,
    }
}

/// A team member as gob sends it (`model.TeamMember`, team_member.go:14). `CreateAt` is another
/// `json:"-"` field gob carries.
pub fn team_member_to_wire(member: &TeamMember) -> wire_model::TeamMember {
    wire_model::TeamMember {
        team_id: member.team_id.clone(),
        user_id: member.user_id.clone(),
        roles: member.roles.clone(),
        delete_at: member.delete_at,
        scheme_guest: member.scheme_guest,
        scheme_user: member.scheme_user,
        scheme_admin: member.scheme_admin,
        explicit_roles: member.explicit_roles.clone(),
        create_at: member.create_at,
    }
}

/// A member a `TeamMemberWillBeAdded` plugin answered with. Every field is a scalar, so unlike
/// [`channel_member_from_wire`] there is nothing gob's omission rule can hide.
pub fn team_member_from_wire(wire: &wire_model::TeamMember) -> TeamMember {
    TeamMember {
        team_id: wire.team_id.clone(),
        user_id: wire.user_id.clone(),
        roles: wire.roles.clone(),
        delete_at: wire.delete_at,
        scheme_guest: wire.scheme_guest,
        scheme_user: wire.scheme_user,
        scheme_admin: wire.scheme_admin,
        explicit_roles: wire.explicit_roles.clone(),
        create_at: wire.create_at,
    }
}

/// A channel as gob sends it, field for field (`model.Channel`, channel.go:64).
pub fn channel_to_wire(channel: &Channel) -> wire_model::Channel {
    wire_model::Channel {
        id: channel.id.clone(),
        create_at: channel.create_at,
        update_at: channel.update_at,
        delete_at: channel.delete_at,
        team_id: channel.team_id.clone(),
        r#type: channel.channel_type.clone(),
        display_name: channel.display_name.clone(),
        name: channel.name.clone(),
        header: channel.header.clone(),
        purpose: channel.purpose.clone(),
        last_post_at: channel.last_post_at,
        total_msg_count: channel.total_msg_count,
        extra_update_at: channel.extra_update_at,
        creator_id: channel.creator_id.clone(),
        scheme_id: channel.scheme_id.clone(),
        props: props_to_wire(channel.props.as_ref()),
        group_constrained: channel.group_constrained,
        auto_translation: channel.auto_translation,
        shared: channel.shared,
        total_msg_count_root: channel.total_msg_count_root,
        policy_id: channel.policy_id.clone(),
        last_root_post_at: channel.last_root_post_at,
        banner_info: channel.banner_info.as_ref().map(|b| {
            Box::new(wire_model::ChannelBannerInfo {
                enabled: b.enabled,
                text: b.text.clone(),
                background_color: b.background_color.clone(),
            })
        }),
        policy_enforced: channel.policy_enforced,
        policy_actions: channel
            .policy_actions
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        policy_is_active: channel.policy_is_active,
        default_category_name: channel.default_category_name.clone(),
        managed_category_name: channel.managed_category_name.clone(),
        discoverable: channel.discoverable,
    }
}

/// A channel a `ChannelWillBeUpdated` plugin answered with.
///
/// **Taken whole, not merged.** Unlike the post and member hooks, Go's client leaves this reply
/// unseeded (`_returns := &Z_ChannelWillBeUpdatedReturns{}`, client_rpc_generated.go:1992), so the
/// replacement is a fresh struct: a field the plugin left out is zero, and a map it left empty is
/// nil. A plugin that answers a channel carrying only its new header has blanked the name.
pub fn channel_from_wire(wire: &wire_model::Channel) -> Channel {
    Channel {
        id: wire.id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        team_id: wire.team_id.clone(),
        channel_type: wire.r#type.clone(),
        display_name: wire.display_name.clone(),
        name: wire.name.clone(),
        header: wire.header.clone(),
        purpose: wire.purpose.clone(),
        last_post_at: wire.last_post_at,
        total_msg_count: wire.total_msg_count,
        extra_update_at: wire.extra_update_at,
        creator_id: wire.creator_id.clone(),
        scheme_id: wire.scheme_id.clone(),
        props: (!wire.props.is_empty()).then(|| props_from_wire(&wire.props)),
        group_constrained: wire.group_constrained,
        auto_translation: wire.auto_translation,
        shared: wire.shared,
        total_msg_count_root: wire.total_msg_count_root,
        policy_id: wire.policy_id.clone(),
        last_root_post_at: wire.last_root_post_at,
        banner_info: wire.banner_info.as_deref().map(|b| ChannelBannerInfo {
            enabled: b.enabled,
            text: b.text.clone(),
            background_color: b.background_color.clone(),
        }),
        policy_enforced: wire.policy_enforced,
        policy_actions: (!wire.policy_actions.is_empty()).then(|| {
            wire.policy_actions
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        }),
        policy_is_active: wire.policy_is_active,
        default_category_name: wire.default_category_name.clone(),
        managed_category_name: wire.managed_category_name.clone(),
        discoverable: wire.discoverable,
    }
}

/// A draft as gob sends it (`model.Draft`, draft.go:13).
///
/// **Not `Metadata`.** `mm-plugin`'s `PostMetadata` is not converted either way ([D-931]), so a
/// draft the client sent with metadata reaches the plugin without it. Go has no `ForPlugin` for
/// drafts and would send it; clients do not put metadata on a draft they save.
pub fn draft_to_wire(draft: &Draft) -> wire_model::Draft {
    wire_model::Draft {
        create_at: draft.create_at,
        update_at: draft.update_at,
        delete_at: draft.delete_at,
        user_id: draft.user_id.clone(),
        channel_id: draft.channel_id.clone(),
        root_id: draft.root_id.clone(),
        message: draft.message.clone(),
        r#type: draft.draft_type.clone(),
        props: props_to_wire(draft.props.as_ref()),
        file_ids: draft.file_ids.clone().unwrap_or_default(),
        metadata: None,
        priority: props_to_wire(draft.priority.as_ref()),
    }
}

/// A draft a `DraftWillBeUpserted` plugin answered with, **taken whole**: like
/// `ChannelWillBeUpdated`, Go leaves this reply unseeded (client_rpc_generated.go:2153), so a
/// field the plugin left out is zero and a map or list it left empty is nil. `metadata` is kept
/// from the draft that was sent, since no wire metadata is converted back ([D-931]).
pub fn draft_from_wire(wire: &wire_model::Draft, source: &Draft) -> Draft {
    Draft {
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        root_id: wire.root_id.clone(),
        message: wire.message.clone(),
        draft_type: wire.r#type.clone(),
        props: (!wire.props.is_empty()).then(|| props_from_wire(&wire.props)),
        file_ids: (!wire.file_ids.is_empty()).then(|| wire.file_ids.clone()),
        metadata: source.metadata.clone(),
        priority: (!wire.priority.is_empty()).then(|| props_from_wire(&wire.priority)),
    }
}

/// A scheduled post as gob sends it (`model.ScheduledPost`, scheduled_post.go:31): the embedded
/// `Draft` as one field named after its type, then the six of its own. The draft half is
/// [`draft_to_wire`], so it carries no `Metadata` either ([D-931]).
pub fn scheduled_post_to_wire(scheduled_post: &ScheduledPost) -> wire_model::ScheduledPost {
    wire_model::ScheduledPost {
        draft: draft_to_wire(&scheduled_post.draft),
        id: scheduled_post.id.clone(),
        scheduled_at: scheduled_post.scheduled_at,
        processed_at: scheduled_post.processed_at,
        error_code: scheduled_post.error_code.clone(),
        repeat_type: scheduled_post.repeat_type.clone(),
        repeat_timezone: scheduled_post.repeat_timezone.clone(),
    }
}

/// A scheduled post a `ScheduledPostWillBeCreated` plugin answered with, **taken whole**: Go
/// leaves `Z_ScheduledPostWillBeCreatedReturns` unseeded (client_rpc_generated.go:2099), so a
/// field the plugin left out is zero — the id included, which `CreateScheduledPost`'s second
/// `PreSave` then mints afresh. `metadata` is kept from the value that was sent, as
/// [`draft_from_wire`] keeps it.
pub fn scheduled_post_from_wire(
    wire: &wire_model::ScheduledPost,
    source: &ScheduledPost,
) -> ScheduledPost {
    ScheduledPost {
        draft: draft_from_wire(&wire.draft, &source.draft),
        id: wire.id.clone(),
        scheduled_at: wire.scheduled_at,
        processed_at: wire.processed_at,
        error_code: wire.error_code.clone(),
        repeat_type: wire.repeat_type.clone(),
        repeat_timezone: wire.repeat_timezone.clone(),
    }
}

/// A file info as gob sends it, field for field (`model.FileInfo`, file_info.go:48). `Path`,
/// `ThumbnailPath`, `PreviewPath` and `Content` are `json:"-"` and gob carries them anyway, so a
/// plugin sees where the bytes live.
pub fn file_info_to_wire(info: &FileInfo) -> wire_model::FileInfo {
    wire_model::FileInfo {
        id: info.id.clone(),
        creator_id: info.creator_id.clone(),
        post_id: info.post_id.clone(),
        channel_id: info.channel_id.clone(),
        create_at: info.create_at,
        update_at: info.update_at,
        delete_at: info.delete_at,
        path: info.path.clone(),
        thumbnail_path: info.thumbnail_path.clone(),
        preview_path: info.preview_path.clone(),
        name: info.name.clone(),
        extension: info.extension.clone(),
        size: info.size,
        mime_type: info.mime_type.clone(),
        width: info.width,
        height: info.height,
        has_preview_image: info.has_preview_image,
        mini_preview: info.mini_preview.clone(),
        content: info.content.clone(),
        remote_id: info.remote_id.clone(),
        archived: info.archived,
    }
}

/// The model a `FileWillBeUploaded` answer merges back into — every field, because the host
/// client already decoded the reply **into** a copy of what it sent (see
/// [`mm_plugin::rpc::HooksClient::file_will_be_uploaded`]), so a field the plugin left out holds
/// the caller's value by the time it gets here.
pub fn file_info_from_wire(wire: &wire_model::FileInfo) -> FileInfo {
    FileInfo {
        id: wire.id.clone(),
        creator_id: wire.creator_id.clone(),
        post_id: wire.post_id.clone(),
        channel_id: wire.channel_id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        path: wire.path.clone(),
        thumbnail_path: wire.thumbnail_path.clone(),
        preview_path: wire.preview_path.clone(),
        name: wire.name.clone(),
        extension: wire.extension.clone(),
        size: wire.size,
        mime_type: wire.mime_type.clone(),
        width: wire.width,
        height: wire.height,
        has_preview_image: wire.has_preview_image,
        mini_preview: wire.mini_preview.clone(),
        content: wire.content.clone(),
        remote_id: wire.remote_id.clone(),
        archived: wire.archived,
    }
}

/// The one `io.Reader` `runPluginsHook` lends **every** plugin in turn: a plugin that reads the
/// upload leaves the next one what is left, which is usually nothing.
#[derive(Clone)]
struct SharedReader {
    data: Arc<Vec<u8>>,
    at: Arc<std::sync::Mutex<usize>>,
}

impl tokio::io::AsyncRead for SharedReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut at = self
            .at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rest = self.data.get(*at..).unwrap_or_default();
        let n = rest.len().min(buf.remaining());
        buf.put_slice(&rest[..n]);
        *at += n;
        std::task::Poll::Ready(Ok(()))
    }
}

/// The one pipe writer `runPluginsHook` lends every plugin: what each writes is appended, and the
/// whole is what replaces the file.
#[derive(Clone, Default)]
struct SharedBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for SharedBuffer {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(data);
        std::task::Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// `model.FileDownloadType` (model/file_info.go:27): which of the four read routes is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDownloadType {
    /// `GET /api/v4/files/{file_id}`.
    File,
    /// `GET /api/v4/files/{file_id}/thumbnail`.
    Thumbnail,
    /// `GET /api/v4/files/{file_id}/preview`.
    Preview,
    /// `GET /files/{file_id}/public`, which has no session.
    Public,
}

impl FileDownloadType {
    /// The string Go sends, to the plugin and in the websocket event.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Thumbnail => "thumbnail",
            Self::Preview => "preview",
            Self::Public => "public",
        }
    }
}

/// `model.PluginSettingsDefaultHookTimeoutSeconds` (model/config.go:273) — the budget
/// `RunFileWillBeDownloadedHook` gives the **whole** dispatch, not each plugin.
const FILE_DOWNLOAD_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A post as gob sends it, field for field.
///
/// **Not** `Metadata`: every call site passes [`Post::for_plugin`] first, which nils it
/// (`model/post.go:1355`), and no hook site here reads a replacement's metadata back — see
/// [`App::run_guarded_message_will_be_posted`].
///
/// `Participants` **is** sent now that [`user_to_wire`] exists. It is nil on every path that
/// reaches these hooks — only the thread reads fill it — so this changes no byte any plugin has
/// seen; it removes the silent gap rather than a live divergence.
/// `model.PushNotification` as gob carries it to `NotificationWillBePushed` — every field,
/// including the two `json:"-"` ones (`PostType`, `ChannelType`) that only plugins see.
pub fn push_notification_to_wire(
    msg: &mm_model::push_notification::PushNotification,
) -> wire_model::PushNotification {
    wire_model::PushNotification {
        ack_id: msg.ack_id.clone(),
        platform: msg.platform.clone(),
        server_id: msg.server_id.clone(),
        device_id: msg.device_id.clone(),
        post_id: msg.post_id.clone(),
        category: msg.category.clone(),
        sound: msg.sound.clone(),
        message: msg.message.clone(),
        badge: msg.badge,
        content_available: msg.content_available,
        team_id: msg.team_id.clone(),
        channel_id: msg.channel_id.clone(),
        root_id: msg.root_id.clone(),
        channel_name: msg.channel_name.clone(),
        r#type: msg.type_.clone(),
        sub_type: msg.sub_type.0.clone(),
        transport: msg.transport.0.clone(),
        sender_id: msg.sender_id.clone(),
        sender_name: msg.sender_name.clone(),
        override_username: msg.override_username.clone(),
        override_icon_url: msg.override_icon_url.clone(),
        from_webhook: msg.from_webhook.clone(),
        version: msg.version.clone(),
        is_crt_enabled: msg.is_crt_enabled,
        is_id_loaded: msg.is_id_loaded,
        post_type: msg.post_type.clone(),
        channel_type: msg.channel_type.clone(),
        signature: msg.signature.clone(),
    }
}

/// The inverse of [`push_notification_to_wire`]: a plugin's replacement, whole.
pub fn push_notification_from_wire(
    wire: &wire_model::PushNotification,
) -> mm_model::push_notification::PushNotification {
    use mm_model::push_notification::{PushNotification, PushSubType, PushTransport};
    PushNotification {
        ack_id: wire.ack_id.clone(),
        platform: wire.platform.clone(),
        server_id: wire.server_id.clone(),
        device_id: wire.device_id.clone(),
        post_id: wire.post_id.clone(),
        category: wire.category.clone(),
        sound: wire.sound.clone(),
        message: wire.message.clone(),
        badge: wire.badge,
        content_available: wire.content_available,
        team_id: wire.team_id.clone(),
        channel_id: wire.channel_id.clone(),
        root_id: wire.root_id.clone(),
        channel_name: wire.channel_name.clone(),
        type_: wire.r#type.clone(),
        sub_type: PushSubType(wire.sub_type.clone()),
        transport: PushTransport(wire.transport.clone()),
        sender_id: wire.sender_id.clone(),
        sender_name: wire.sender_name.clone(),
        override_username: wire.override_username.clone(),
        override_icon_url: wire.override_icon_url.clone(),
        from_webhook: wire.from_webhook.clone(),
        version: wire.version.clone(),
        is_crt_enabled: wire.is_crt_enabled,
        is_id_loaded: wire.is_id_loaded,
        post_type: wire.post_type.clone(),
        channel_type: wire.channel_type.clone(),
        signature: wire.signature.clone(),
    }
}

pub fn post_to_wire(post: &Post) -> wire_model::Post {
    wire_model::Post {
        id: post.id.clone(),
        create_at: post.create_at,
        update_at: post.update_at,
        edit_at: post.edit_at,
        delete_at: post.delete_at,
        is_pinned: post.is_pinned,
        user_id: post.user_id.clone(),
        channel_id: post.channel_id.clone(),
        root_id: post.root_id.clone(),
        original_id: post.original_id.clone(),
        message: post.message.clone(),
        message_source: post.message_source.clone(),
        r#type: post.post_type.clone(),
        props: props_to_wire(post.props.as_ref()),
        hashtags: post.hashtags.clone(),
        filenames: post.filenames.clone(),
        file_ids: post.file_ids.clone().unwrap_or_default(),
        pending_post_id: post.pending_post_id.clone(),
        has_reactions: post.has_reactions,
        remote_id: post.remote_id.clone(),
        reply_count: post.reply_count,
        last_reply_at: post.last_reply_at,
        participants: post
            .participants
            .iter()
            .flatten()
            .map(user_to_wire)
            .collect(),
        is_following: post.is_following,
        metadata: None,
    }
}

/// A post a plugin answered with, back into the model.
///
/// `source` is the post the call was made with. gob cannot tell an empty map or slice from an
/// absent one — it omits both — so a plugin that left `Props` or `FileIds` alone is
/// indistinguishable from one that emptied them, and Go's merge keeps the caller's value. The
/// three `Option` fields `mm-model` models (`props`, `file_ids`, `participants`) therefore come
/// from `source` when what came back is empty; taking `Some(empty)` instead would turn a post
/// whose `props` was `null` into one whose `props` is `{}` on the wire.
pub fn post_from_wire(wire: &wire_model::Post, source: &Post) -> Post {
    Post {
        id: wire.id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        edit_at: wire.edit_at,
        delete_at: wire.delete_at,
        is_pinned: wire.is_pinned,
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        root_id: wire.root_id.clone(),
        original_id: wire.original_id.clone(),
        message: wire.message.clone(),
        message_source: wire.message_source.clone(),
        post_type: wire.r#type.clone(),
        props: if wire.props.is_empty() {
            source.props.clone()
        } else {
            Some(props_from_wire(&wire.props))
        },
        hashtags: wire.hashtags.clone(),
        filenames: wire.filenames.clone(),
        file_ids: if wire.file_ids.is_empty() {
            source.file_ids.clone()
        } else {
            Some(wire.file_ids.clone())
        },
        pending_post_id: wire.pending_post_id.clone(),
        has_reactions: wire.has_reactions,
        remote_id: wire.remote_id.clone(),
        reply_count: wire.reply_count,
        last_reply_at: wire.last_reply_at,
        participants: source.participants.clone(),
        is_following: wire.is_following,
        // `ForPlugin` nils it on the way out and every call site restores it on the way back.
        metadata: None,
    }
}

/// A post a plugin answered `MessagesWillBeConsumed` with, taken **whole**.
///
/// Unlike [`post_from_wire`], nothing comes from the post the call was made with. Go decodes the
/// reply into a fresh `Z_MessagesWillBeConsumedReturns` (client_rpc.go:977, :1015) and assigns
/// each element straight into the map — `posts[postReplacement.Id] = postReplacement`
/// (post.go:3037). The comment above the `WithContext` variant promises "decoding the returned
/// post into the original one to avoid the unintentional removal of fields by older plugins";
/// the code beneath it does no such thing, and the wire is what the code does. So a field the
/// plugin left out is its zero value: `Props`, `FileIds` and `Participants` come back nil, which
/// is `None` here and an absent key to the client. Only `Metadata` is put back, by the caller,
/// from the value it held before the hook.
pub fn post_from_wire_whole(wire: &wire_model::Post) -> Post {
    Post {
        id: wire.id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        edit_at: wire.edit_at,
        delete_at: wire.delete_at,
        is_pinned: wire.is_pinned,
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        root_id: wire.root_id.clone(),
        original_id: wire.original_id.clone(),
        message: wire.message.clone(),
        message_source: wire.message_source.clone(),
        post_type: wire.r#type.clone(),
        props: (!wire.props.is_empty()).then(|| props_from_wire(&wire.props)),
        hashtags: wire.hashtags.clone(),
        filenames: wire.filenames.clone(),
        file_ids: (!wire.file_ids.is_empty()).then(|| wire.file_ids.clone()),
        pending_post_id: wire.pending_post_id.clone(),
        has_reactions: wire.has_reactions,
        remote_id: wire.remote_id.clone(),
        reply_count: wire.reply_count,
        last_reply_at: wire.last_reply_at,
        participants: (!wire.participants.is_empty())
            .then(|| wire.participants.iter().map(user_from_wire).collect()),
        is_following: wire.is_following,
        metadata: None,
    }
}

/// `metadataByPostID` (post.go:3005): the posts the hook is about — every one that is not
/// burn-on-read — keyed by id, each with the metadata that is put back on its replacement.
fn consumable_metadata(
    posts: &PostMap,
) -> std::collections::BTreeMap<String, Option<PostMetadata>> {
    posts
        .iter()
        .filter(|(_, post)| post.post_type != POST_TYPE_BURN_ON_READ)
        .map(|(id, post)| (id.clone(), post.metadata.clone()))
        .collect()
}

/// `rebuildPostsSlice` (post.go:3007): the consumable posts, each `ForPlugin`-ed afresh, as the
/// next plugin is handed them. Go walks a map, so its order is whatever the runtime gives; this
/// walks the `BTreeMap`, so it is by id. Neither is a promise to the plugin.
fn consumed_slice(
    posts: &PostMap,
    metadata_by_id: &std::collections::BTreeMap<String, Option<PostMetadata>>,
) -> Vec<wire_model::Post> {
    posts
        .iter()
        .filter(|(id, _)| metadata_by_id.contains_key(*id))
        .map(|(_, post)| post_to_wire(&post.for_plugin()))
        .collect()
}

/// `applyReplacements` (post.go:3027): each answer lands on the map under its **own** id, whole,
/// with the pre-hook metadata put back; an id the hook was not asked about is ignored ("if the
/// plugin returned a post with a new id, ignore it"). Go also skips a nil element, which gob
/// refuses to encode in the first place, so none reaches here.
fn apply_consumed_replacements(
    posts: &mut PostMap,
    metadata_by_id: &std::collections::BTreeMap<String, Option<PostMetadata>>,
    replacements: Vec<wire_model::Post>,
) {
    for replacement in replacements {
        let Some(metadata) = metadata_by_id.get(&replacement.id) else {
            continue;
        };
        let mut post = post_from_wire_whole(&replacement);
        post.metadata = metadata.clone();
        posts.insert(replacement.id.clone(), post);
    }
}

/// A reaction as gob sends it. Go passes the `*model.Reaction` through **uncloned**
/// (`reaction.go:108`), so a plugin observing it sees whatever the calling goroutine has;
/// nothing here relies on that, and the value is copied before the task is spawned.
pub fn reaction_to_wire(reaction: &Reaction) -> wire_model::Reaction {
    wire_model::Reaction {
        user_id: reaction.user_id.clone(),
        post_id: reaction.post_id.clone(),
        emoji_name: reaction.emoji_name.clone(),
        create_at: reaction.create_at,
        update_at: reaction.update_at,
        delete_at: reaction.delete_at,
        remote_id: reaction.remote_id.clone(),
        channel_id: reaction.channel_id.clone(),
    }
}

// -------------------------------------------------------------------------------------------
// The dispatchers
// -------------------------------------------------------------------------------------------

/// `a.Srv().Go(func() { ch.RunMultiHook(...) })`: the notification shape. The task is detached,
/// as Go's is; `Srv().Go` additionally makes the server wait for it at shutdown, which this does
/// not — a hook still running when the process exits is lost here and completed there.
fn spawn_multi_hook<F, Fut>(environment: Arc<PluginsEnvironment>, hook_id: usize, mut f: F)
where
    F: FnMut(Arc<HooksClient>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        environment
            .run_multi_plugin_hook(hook_id, |hooks, _manifest| {
                let call = f(hooks);
                async move {
                    call.await;
                    true
                }
            })
            .await;
    });
}

/// The gate in front of `GenerateSupportData` (app/support_packet.go:107): a plugin that
/// declared the `support_packet` prop is called only when the admin ticked it.
pub(crate) fn support_data_requested(
    manifest: &mm_model::manifest::Manifest,
    plugin_packets: &[String],
) -> bool {
    let declared = manifest
        .props
        .as_ref()
        .is_some_and(|props| props.contains_key("support_packet"));
    !declared || plugin_packets.contains(&manifest.id)
}

/// `model.NewAppError(caller, "app.plugin.inactive_guard.app_error", nil, "", 503)` —
/// `logAndErrPluginsDisabled` and `logAndErrPluginInactive` answer the same error
/// (guarded_hooks.go:78, :63).
fn inactive_guard_error(caller: &str) -> Box<AppError> {
    AppError::boxed(
        caller,
        "app.plugin.inactive_guard.app_error",
        None,
        String::new(),
        503,
    )
}

/// `model.NewAppError(caller, "app.plugin.guard_hook_failed.app_error", {"PluginID": id}, "",
/// 503)` — `appErrHookFailed` (guarded_hooks.go:88), the fail-closed answer to a guard whose RPC
/// broke.
fn guard_hook_failed_error(plugin_id: &str, caller: &str) -> Box<AppError> {
    AppError::boxed(
        caller,
        "app.plugin.guard_hook_failed.app_error",
        Some(std::collections::HashMap::from([(
            "PluginID".to_owned(),
            serde_json::Value::String(plugin_id.to_owned()),
        )])),
        String::new(),
        503,
    )
}

/// The rejection a reason becomes: the reason *is* the id, behind [`REJECTED_PREFIX`], unless it
/// is [`DISMISS_POST_ERROR`], which stands alone (guarded_hooks.go:134, :195).
///
/// Not a translation key and not a parameter — a client sees the plugin's own text in the error's
/// `id` and, since no bundle has that key, in its `message` too.
/// The rejection the **membership** hooks build, which is the opposite arrangement to
/// [`rejection_error`]: a real translation key with the reason as a `Reason` parameter
/// (guarded_hooks.go:249, team.go:813). A client of a translating server therefore reads
/// "Adding channel member rejected by plugin: <reason>" here, and the plugin's bare text in the
/// error *id* on the post paths.
fn member_rejection_error(caller: &str, id: &str, reason: &str) -> Box<AppError> {
    AppError::boxed(
        caller,
        id,
        Some(std::collections::HashMap::from([(
            "Reason".to_owned(),
            serde_json::Value::String(reason.to_owned()),
        )])),
        String::new(),
        400,
    )
}

fn rejection_error(reason: &str, caller: &str) -> Box<AppError> {
    let id = if reason == DISMISS_POST_ERROR {
        DISMISS_POST_ERROR.to_owned()
    } else {
        format!("{REJECTED_PREFIX}{reason}")
    };
    AppError::boxed(caller, id, None, String::new(), 400)
}

impl App {
    /// The environment a hook may run in: `Channels.GetPluginsEnvironment()` (app/plugin.go:47),
    /// and `None` whenever this process is not the host (docs/PLUGIN_PLAN.md, D6).
    pub(crate) fn hook_environment(&self) -> Option<Arc<PluginsEnvironment>> {
        if !self.plugin_host().hosted() {
            return None;
        }
        self.plugins_environment()
    }

    /// Port of `resolveGuards` (guarded_hooks.go:37): the plugin ids claiming this channel,
    /// sorted, plus the fail-closed refusal when the channel has guards the plugin system cannot
    /// run. A channel with no guards answers `(vec![], None)`, and both phases below then behave
    /// exactly like a plain `RunMultiHook` — which is every channel on any server here, because
    /// `RegisterChannelGuard` is one of the API methods Phase 6 still owes.
    ///
    /// Go reads a cache loaded at start-up; this reads the table
    /// ([`mm_store::channel_guard_store`]), which is the same answer without a reload path
    /// nothing in this process can trigger. A store failure is "no guards", the state Go's cache
    /// is left in when its own load fails (`app/channel_guards.go:30`) — not a refusal, which
    /// would turn a database blip into a 503 on every post.
    async fn resolve_guards(
        &self,
        channel_id: &str,
        caller: &str,
    ) -> (Vec<String>, Option<Box<AppError>>) {
        use mm_store::channel_guard_store::ChannelGuardStore as _;

        let guards = match self
            .store()
            .channel_guard()
            .get_for_channel(channel_id)
            .await
        {
            Ok(guards) => guards,
            Err(err) => {
                tracing::warn!(error = %err, channel_id, "reading the channel guards failed");
                return (Vec::new(), None);
            }
        };
        if guards.is_empty() {
            return (Vec::new(), None);
        }
        let mut sorted: Vec<String> = guards.into_iter().map(|g| g.plugin_id).collect();
        sorted.sort();

        let Some(environment) = self.hook_environment() else {
            tracing::error!(
                error_id = "plugins_disabled_with_guards",
                channel_id,
                caller,
                "Channel guard rejected operation: plugin system is disabled but guards exist for this channel",
            );
            return (sorted, Some(inactive_guard_error(caller)));
        };
        let inactive: Vec<&str> = sorted
            .iter()
            .filter(|id| !environment.is_active(id))
            .map(String::as_str)
            .collect();
        if !inactive.is_empty() {
            tracing::error!(
                error_id = "guard_plugin_inactive",
                channel_id,
                caller,
                plugin_ids = ?inactive,
                "Channel guard rejected operation: claiming plugin is not active",
            );
            return (sorted, Some(inactive_guard_error(caller)));
        }
        (sorted, None)
    }

    /// Port of `runGuardedMessageWillBePosted` (guarded_hooks.go:108) — hook 5,
    /// `MessageWillBePosted`, which runs before `CreateAt` is filled, before the embeds and
    /// before the save.
    ///
    /// **A rejection is `reason != ""` and nothing else.** A plugin answering no replacement with
    /// no reason has simply declined, and iteration continues — the opposite of phase A of
    /// [`App::run_guarded_message_will_be_updated`]. Unifying the two changes what a client sees.
    ///
    /// Go carries the pre-hook `Metadata` across a replacement, keeping the replacement's own
    /// when it has one and overwriting only its `Priority`. Here the pre-hook value is always
    /// restored, because a plugin's `PostMetadata` is not converted back ([D-931]); `ForPlugin`
    /// nils metadata on the way out, so a plugin has to invent one to tell the difference.
    pub(crate) async fn run_guarded_message_will_be_posted(
        &self,
        ctx: &HookContext,
        post: Post,
    ) -> Result<Post, Box<AppError>> {
        const CALLER: &str = "createPost";

        // Under the Go host the guards' plugins run in the *other* process, which answers its own
        // requests with them; refusing here would be a 503 where Go answers 201, and reading the
        // table on every create would be a query for nothing. So nothing happens, which is what
        // "keep today's behaviour under the Go host" means ([D-932], [D-933]).
        if !self.plugin_host().hosted() {
            return Ok(post);
        }

        // `resolveGuards` runs before anything else, and before the environment is consulted: a
        // guarded channel is a 503 precisely when there is no environment to run the guard in —
        // here, `PluginSettings.Enable` off while this process hosts.
        let (guards, rejected) = self.resolve_guards(&post.channel_id, CALLER).await;
        if let Some(err) = rejected {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(post);
        };

        let metadata = post.metadata.clone();
        let mut post = post;

        // Phase A: `RunMultiHookExcluding`, fail-open, over the plugins that do not guard this
        // channel. Each call sends the accumulated post, freshly `ForPlugin`-ed.
        for (hooks, manifest) in environment.hooks_implementing(hook_id::MESSAGE_WILL_BE_POSTED) {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .message_will_be_posted(wire_plugin::Z_MessageWillBePostedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(post_to_wire(&post.for_plugin()))),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(rejection_error(&returns.b, CALLER));
            }
            if let Some(replacement) = returns.a.as_deref() {
                post = post_from_wire(replacement, &post);
            }
        }

        // Phase B: each guard claimant in `PluginId` order, fail-closed on a transport failure
        // and on a plugin that went inactive since `resolveGuards` read the table.
        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %post.channel_id,
                    caller = CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error("CreatePost"));
            };
            let (returns, rpc_err) = hooks
                .message_will_be_posted_with_rpc_err(wire_plugin::Z_MessageWillBePostedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(post_to_wire(&post.for_plugin()))),
                })
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, "CreatePost"));
            }
            if !returns.b.is_empty() {
                return Err(rejection_error(&returns.b, CALLER));
            }
            if let Some(replacement) = returns.a.as_deref() {
                post = post_from_wire(replacement, &post);
            }
        }

        post.metadata = metadata;
        Ok(post)
    }

    /// Port of `runGuardedMessageWillBeUpdated` (guarded_hooks.go:182) — hook 6,
    /// `MessageWillBeUpdated`, between `FillInPostProps` and the store update.
    ///
    /// **Phase A rejects on a nil post, not on a reason.** Go assigns the hook's answer straight
    /// back into `newPost` every iteration and stops when it is nil, so a plugin answering
    /// `(nil, "")` rejects the edit with the *empty* reason and the client sees the id
    /// `"Post rejected by plugin. "`, trailing space and all. Phase B is the other way round:
    /// there a nil replacement with no reason means "no opinion" and iteration continues.
    ///
    /// Unlike `MessageWillBePosted`, a plugin's answer **replaces** the post rather than merging
    /// into it (`client_rpc.go`, and [`mm_plugin::rpc::HooksClient::message_will_be_updated`]).
    /// The caller restores the metadata itself, as Go does, right after this returns.
    pub(crate) async fn run_guarded_message_will_be_updated(
        &self,
        ctx: &HookContext,
        new_post: Post,
        old_post: &Post,
    ) -> Result<Post, Box<AppError>> {
        const CALLER: &str = "UpdatePost";

        // See [`App::run_guarded_message_will_be_posted`]: under the Go host, nothing.
        if !self.plugin_host().hosted() {
            return Ok(new_post);
        }

        let (guards, rejected) = self.resolve_guards(&old_post.channel_id, CALLER).await;
        if let Some(err) = rejected {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(new_post);
        };

        let old_wire = post_to_wire(&old_post.for_plugin());
        let mut post = Some(new_post);
        let mut reason = String::new();

        for (hooks, manifest) in environment.hooks_implementing(hook_id::MESSAGE_WILL_BE_UPDATED) {
            if guards.contains(&manifest.id) {
                continue;
            }
            let Some(current) = post.as_ref() else { break };
            let returns = hooks
                .message_will_be_updated(wire_plugin::Z_MessageWillBeUpdatedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(post_to_wire(&current.for_plugin()))),
                    c: Some(Box::new(old_wire.clone())),
                })
                .await;
            reason = returns.b;
            post = returns.a.as_deref().map(|p| post_from_wire(p, current));
        }
        let Some(mut current) = post else {
            return Err(rejection_error(&reason, CALLER));
        };

        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %old_post.channel_id,
                    caller = CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(CALLER));
            };
            let (returns, rpc_err) = hooks
                .message_will_be_updated_with_rpc_err(wire_plugin::Z_MessageWillBeUpdatedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(post_to_wire(&current.for_plugin()))),
                    c: Some(Box::new(old_wire.clone())),
                })
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, CALLER));
            }
            if !returns.b.is_empty() {
                return Err(rejection_error(&returns.b, CALLER));
            }
            if let Some(replacement) = returns.a.as_deref() {
                current = post_from_wire(replacement, &current);
            }
        }
        Ok(current)
    }

    /// `MessageHasBeenPosted` (hook 7) — post.go:430. The clone is taken here, on the calling
    /// goroutine, exactly as Go takes `rpost.ForPlugin()` outside its `Srv().Go`.
    pub(crate) fn message_has_been_posted(&self, ctx: &HookContext, post: &Post) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        if post.post_type == POST_TYPE_BURN_ON_READ {
            return;
        }
        let (context, wire) = (ctx.boxed_wire(), post_to_wire(&post.for_plugin()));
        spawn_multi_hook(
            environment,
            hook_id::MESSAGE_HAS_BEEN_POSTED,
            move |hooks| {
                let args = wire_plugin::Z_MessageHasBeenPostedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.message_has_been_posted(args).await;
                }
            },
        );
    }

    /// `MessageHasBeenUpdated` (hook 8) — post.go:1007, `(ctx, newPost, oldPost)` in that order.
    pub(crate) fn message_has_been_updated(
        &self,
        ctx: &HookContext,
        new_post: &Post,
        old_post: &Post,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        if new_post.post_type == POST_TYPE_BURN_ON_READ {
            return;
        }
        let context = ctx.boxed_wire();
        let new_wire = post_to_wire(&new_post.for_plugin());
        let old_wire = post_to_wire(&old_post.for_plugin());
        spawn_multi_hook(
            environment,
            hook_id::MESSAGE_HAS_BEEN_UPDATED,
            move |hooks| {
                let args = wire_plugin::Z_MessageHasBeenUpdatedArgs {
                    a: context.clone(),
                    b: Some(Box::new(new_wire.clone())),
                    c: Some(Box::new(old_wire.clone())),
                };
                async move {
                    hooks.message_has_been_updated(args).await;
                }
            },
        );
    }

    /// `MessageHasBeenDeleted` (hook 37) — post.go:3393, inside `CleanUpAfterPostDeletion`,
    /// after the two `post_deleted` events and before `RemoveNotifications`. There is **no**
    /// burn-on-read gate on this one, unlike its three siblings.
    pub(crate) fn message_has_been_deleted(&self, ctx: &HookContext, post: &Post) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), post_to_wire(&post.for_plugin()));
        spawn_multi_hook(
            environment,
            hook_id::MESSAGE_HAS_BEEN_DELETED,
            move |hooks| {
                let args = wire_plugin::Z_MessageHasBeenDeletedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.message_has_been_deleted(args).await;
                }
            },
        );
    }

    /// `ReactionHasBeenAdded` (hook 18) — reaction.go:105, after the save and the cache
    /// invalidation, before `sendReactionEvent`.
    pub(crate) fn reaction_has_been_added(&self, ctx: &HookContext, reaction: &Reaction) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), reaction_to_wire(reaction));
        spawn_multi_hook(
            environment,
            hook_id::REACTION_HAS_BEEN_ADDED,
            move |hooks| {
                let args = wire_plugin::Z_ReactionHasBeenAddedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.reaction_has_been_added(args).await;
                }
            },
        );
    }

    /// `ReactionHasBeenRemoved` (hook 19) — reaction.go:187, in the same place on the delete
    /// path.
    pub(crate) fn reaction_has_been_removed(&self, ctx: &HookContext, reaction: &Reaction) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), reaction_to_wire(reaction));
        spawn_multi_hook(
            environment,
            hook_id::REACTION_HAS_BEEN_REMOVED,
            move |hooks| {
                let args = wire_plugin::Z_ReactionHasBeenRemovedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.reaction_has_been_removed(args).await;
                }
            },
        );
    }

    /// Port of `runGuardedChannelMemberWillBeAdded` (guarded_hooks.go:239) — hook 49,
    /// `ChannelMemberWillBeAdded`, between the new member's flags and `SaveMember`.
    ///
    /// **A rejection is `reason != ""`**, as in
    /// [`App::run_guarded_message_will_be_posted`] and unlike phase A of
    /// [`App::run_guarded_message_will_be_updated`] — but the error is a real translation key
    /// with a `Reason` **parameter**, `app.channel.add_user.to.channel.rejected_by_plugin`, not
    /// the reason concatenated into the id. The two families of rejecting hook disagree about
    /// that, and a client sees the difference.
    ///
    /// Go's three caller names are not the same string: `resolveGuards` is told
    /// `AddUserToChannel` and the rejection uses it, while phase B's two failures say
    /// `addUserToChannel`, lower-case. `Where` is `json:"-"`, so this is invisible on the wire
    /// and kept because it is what the source says.
    pub(crate) async fn run_guarded_channel_member_will_be_added(
        &self,
        ctx: &HookContext,
        channel_id: &str,
        member: ChannelMember,
    ) -> Result<ChannelMember, Box<AppError>> {
        const CALLER: &str = "AddUserToChannel";
        /// Phase B's caller name, which Go spells with a lower-case first letter.
        const PHASE_B_CALLER: &str = "addUserToChannel";

        // See [`App::run_guarded_message_will_be_posted`]: under the Go host, nothing at all —
        // not even the guard read, because the guards' plugins live in the other process.
        if !self.plugin_host().hosted() {
            return Ok(member);
        }

        let (guards, rejected) = self.resolve_guards(channel_id, CALLER).await;
        if let Some(err) = rejected {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(member);
        };

        let mut member = member;

        // Phase A: `RunMultiHookExcluding`, fail-open, over the plugins that do not guard this
        // channel.
        for (hooks, manifest) in
            environment.hooks_implementing(hook_id::CHANNEL_MEMBER_WILL_BE_ADDED)
        {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .channel_member_will_be_added(wire_plugin::Z_ChannelMemberWillBeAddedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(channel_member_to_wire(&member))),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(member_rejection_error(
                    CALLER,
                    "app.channel.add_user.to.channel.rejected_by_plugin",
                    &returns.b,
                ));
            }
            if let Some(replacement) = returns.a.as_deref() {
                member = channel_member_from_wire(replacement, &member);
            }
        }

        // Phase B: each guard claimant in `PluginId` order, fail-closed.
        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id,
                    caller = PHASE_B_CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(PHASE_B_CALLER));
            };
            let (returns, rpc_err) = hooks
                .channel_member_will_be_added_with_rpc_err(
                    wire_plugin::Z_ChannelMemberWillBeAddedArgs {
                        a: ctx.boxed_wire(),
                        b: Some(Box::new(channel_member_to_wire(&member))),
                    },
                )
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, PHASE_B_CALLER));
            }
            if !returns.b.is_empty() {
                return Err(member_rejection_error(
                    CALLER,
                    "app.channel.add_user.to.channel.rejected_by_plugin",
                    &returns.b,
                ));
            }
            if let Some(replacement) = returns.a.as_deref() {
                member = channel_member_from_wire(replacement, &member);
            }
        }

        Ok(member)
    }

    /// `TeamMemberWillBeAdded` (hook 50) — team.go:800, inside the `preSaveHook` that
    /// `TeamService.JoinUserToTeam` applies immediately before `SaveMember` **and** before the
    /// revival `UpdateMember` (app/teams/teams.go:199, :226).
    ///
    /// **A plain `RunMultiHook`, not the guarded dispatcher.** Channel guards guard channels; a
    /// team has none, so Go calls this one directly and a plugin that fails in transport is
    /// simply skipped, where `ChannelMemberWillBeAdded` would refuse the request.
    pub(crate) async fn run_team_member_will_be_added(
        &self,
        ctx: &HookContext,
        member: TeamMember,
    ) -> Result<TeamMember, Box<AppError>> {
        const CALLER: &str = "JoinUserToTeam";

        let Some(environment) = self.hook_environment() else {
            return Ok(member);
        };
        let mut member = member;
        for (hooks, _manifest) in environment.hooks_implementing(hook_id::TEAM_MEMBER_WILL_BE_ADDED)
        {
            let returns = hooks
                .team_member_will_be_added(wire_plugin::Z_TeamMemberWillBeAddedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(team_member_to_wire(&member))),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(member_rejection_error(
                    CALLER,
                    "app.team.join_user_to_team.rejected_by_plugin",
                    &returns.b,
                ));
            }
            if let Some(replacement) = returns.a.as_deref() {
                member = team_member_from_wire(replacement);
            }
        }
        Ok(member)
    }

    /// `UserHasJoinedChannel` (hook 9) — channel.go:2044, in `AddChannelMember`, after the
    /// `channel.IsSpace()` early return and **before** the join or add-to-channel system post.
    ///
    /// `actor` is `userRequestor`, which is `nil` for a self-add — Go's `opts.UserRequestorID ==
    /// ""`. The other call site (`JoinChannel`, channel.go:2764) always passes `nil`, and it is
    /// not reachable here because `App::join_channel` is not ported; see [D-932].
    pub(crate) fn user_has_joined_channel(
        &self,
        ctx: &HookContext,
        member: &ChannelMember,
        actor: Option<&User>,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let context = ctx.boxed_wire();
        let member = channel_member_to_wire(member);
        let actor = actor.map(|u| Box::new(user_to_wire(u)));
        spawn_multi_hook(
            environment,
            hook_id::USER_HAS_JOINED_CHANNEL,
            move |hooks| {
                let args = wire_plugin::Z_UserHasJoinedChannelArgs {
                    a: context.clone(),
                    b: Some(Box::new(member.clone())),
                    c: actor.clone(),
                };
                async move {
                    hooks.user_has_joined_channel(args).await;
                }
            },
        );
    }

    /// `UserHasLeftChannel` (hook 10) — channel.go:3087, in `removeUserFromChannel`, after the
    /// `channel.IsSpace()` early return and **before** the two `user_removed` events.
    ///
    /// `actor` is the remover, loaded with `a.GetUser(removerUserId)` whose error Go **discards**
    /// — so a remover id that no longer resolves gives a nil actor rather than a failed request.
    /// A self-removal through the API still has a remover id, so nil here means the caller passed
    /// none (the local-mode route, and the channel sweep a team move runs).
    pub(crate) fn user_has_left_channel(
        &self,
        ctx: &HookContext,
        member: &ChannelMember,
        actor: Option<&User>,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let context = ctx.boxed_wire();
        let member = channel_member_to_wire(member);
        let actor = actor.map(|u| Box::new(user_to_wire(u)));
        spawn_multi_hook(environment, hook_id::USER_HAS_LEFT_CHANNEL, move |hooks| {
            let args = wire_plugin::Z_UserHasLeftChannelArgs {
                a: context.clone(),
                b: Some(Box::new(member.clone())),
                c: actor.clone(),
            };
            async move {
                hooks.user_has_left_channel(args).await;
            }
        });
    }

    /// `UserHasJoinedTeam` (hook 11) — team.go:884, after the session-cache clear and **before**
    /// the `added_to_team` websocket event.
    ///
    /// `actor` is `a.GetUser(userRequestorId)` with its error discarded, and nil when the join
    /// carried no requestor — which is every self-join and every invite-id join (team.go:746
    /// passes `""`).
    pub(crate) fn user_has_joined_team(
        &self,
        ctx: &HookContext,
        member: &TeamMember,
        actor: Option<&User>,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let context = ctx.boxed_wire();
        let member = team_member_to_wire(member);
        let actor = actor.map(|u| Box::new(user_to_wire(u)));
        spawn_multi_hook(environment, hook_id::USER_HAS_JOINED_TEAM, move |hooks| {
            let args = wire_plugin::Z_UserHasJoinedTeamArgs {
                a: context.clone(),
                b: Some(Box::new(member.clone())),
                c: actor.clone(),
            };
            async move {
                hooks.user_has_joined_team(args).await;
            }
        });
    }

    /// `UserHasLeftTeam` (hook 12) — team.go:1295, the **first** statement of
    /// `postProcessTeamMemberLeave`, before the user is re-read and before the three writes that
    /// can fail the request. So a removal whose sidebar clear 500s has still told every plugin
    /// the user left.
    pub(crate) fn user_has_left_team(
        &self,
        ctx: &HookContext,
        member: &TeamMember,
        actor: Option<&User>,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let context = ctx.boxed_wire();
        let member = team_member_to_wire(member);
        let actor = actor.map(|u| Box::new(user_to_wire(u)));
        spawn_multi_hook(environment, hook_id::USER_HAS_LEFT_TEAM, move |hooks| {
            let args = wire_plugin::Z_UserHasLeftTeamArgs {
                a: context.clone(),
                b: Some(Box::new(member.clone())),
                c: actor.clone(),
            };
            async move {
                hooks.user_has_left_team(args).await;
            }
        });
    }
    /// Port of `DoLogin`'s first statement (app/login.go:137) — hook 15, `UserWillLogIn`, before
    /// the device ids are validated and before anything is written.
    ///
    /// A plain `RunMultiHook` that **stops at the first rejection**: the closure answers
    /// `rejectionReason == ""`, so a plugin that refuses ends the iteration and the plugins after
    /// it are never asked. No plugin can replace the user — the hook returns a string only.
    ///
    /// The rejection is the post family's arrangement, not the membership one: the reason is
    /// concatenated into the error **id**, `"Login rejected by plugin: " + reason`, at 400. On
    /// `POST /users/login` a client never sees it, because `login`'s deferred mask turns every id
    /// outside its short list into `invalid_credentials_*` at 401; the desktop-token login has no
    /// mask and hands the id over as it is.
    pub(crate) async fn run_user_will_log_in(
        &self,
        ctx: &HookContext,
        user: &User,
    ) -> Result<(), Box<AppError>> {
        let Some(environment) = self.hook_environment() else {
            return Ok(());
        };
        let wire = user_to_wire(user);
        for (hooks, _manifest) in environment.hooks_implementing(hook_id::USER_WILL_LOG_IN) {
            let returns = hooks
                .user_will_log_in(wire_plugin::Z_UserWillLogInArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(wire.clone())),
                })
                .await;
            if !returns.a.is_empty() {
                return Err(AppError::boxed(
                    "DoLogin",
                    format!("Login rejected by plugin: {}", returns.a),
                    None,
                    String::new(),
                    400,
                ));
            }
        }
        Ok(())
    }

    /// `UserHasLoggedIn` (hook 16) — app/login.go:233, the last thing `DoLogin` does, after
    /// `UpdateLastLogin`.
    ///
    /// `ctx` is the one taken at the **top** of `DoLogin`, before `rctx.WithSession(session)`, so
    /// its `SessionId` is whatever session the login request itself carried — usually none — and
    /// never the session this login just created.
    ///
    /// Go hands the goroutine the caller's `*model.User` and the `login` handler goes on to write
    /// the terms-of-service pair into it and `Sanitize` it, so what gob encodes depends on which
    /// goroutine gets there first. The handler's next step is a database read, so in practice the
    /// hook sees the row as `DoLogin` got it — hash included — and that is what is sent here,
    /// from a copy taken before the spawn.
    pub(crate) fn user_has_logged_in(&self, ctx: &HookContext, user: &User) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), user_to_wire(user));
        spawn_multi_hook(environment, hook_id::USER_HAS_LOGGED_IN, move |hooks| {
            let args = wire_plugin::Z_UserHasLoggedInArgs {
                a: context.clone(),
                b: Some(Box::new(wire.clone())),
            };
            async move {
                hooks.user_has_logged_in(args).await;
            }
        });
    }

    /// `UserHasBeenCreated` (hook 17) — app/user.go:420, in `createUserOrGuest` after the
    /// `new_user` broadcast and before the soft-limit log line.
    ///
    /// The user is `ruser`, which `userService.createUser` has already `Sanitize`d: no password,
    /// no auth data, no MFA secret — the one user-carrying hook that sends a sanitised row.
    pub(crate) fn user_has_been_created(&self, ctx: &HookContext, user: &User) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), user_to_wire(user));
        spawn_multi_hook(environment, hook_id::USER_HAS_BEEN_CREATED, move |hooks| {
            let args = wire_plugin::Z_UserHasBeenCreatedArgs {
                a: context.clone(),
                b: Some(Box::new(wire.clone())),
            };
            async move {
                hooks.user_has_been_created(args).await;
            }
        });
    }

    /// `UserHasBeenDeactivated` (hook 36) — app/user.go:1283, at the end of `UpdateActive`'s
    /// deactivating arm, after `sendUpdatedUserEvent`, under `!active && user.DeleteAt != 0`.
    ///
    /// The user is the **caller's** `*model.User`, which `SqlUserStore.Update` has mutated in
    /// place — `PreUpdate`, the protected columns copied back from the old row, and
    /// `Sanitize` — before deep-copying it into `UserUpdate.New`. So it is the same value as the
    /// updated row the event carries, sanitised, and callers pass that.
    pub(crate) fn user_has_been_deactivated(&self, ctx: &HookContext, user: &User) {
        if user.delete_at == 0 {
            return;
        }
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), user_to_wire(user));
        spawn_multi_hook(
            environment,
            hook_id::USER_HAS_BEEN_DEACTIVATED,
            move |hooks| {
                let args = wire_plugin::Z_UserHasBeenDeactivatedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.user_has_been_deactivated(args).await;
                }
            },
        );
    }
    /// Port of `RunFileWillBeDownloadedHook` (app/file.go:2008) — hook 48,
    /// `FileWillBeDownloaded` — for the four read routes, after the permission and ABAC checks.
    /// Answers the rejection reason, `""` to let the download through.
    ///
    /// A `RunMultiHook` that stops at the first plugin to refuse, run on its own task under a
    /// **thirty-second budget for the whole dispatch**. When the budget runs out the download is
    /// refused with `api.file.get_file.plugin_hook_timeout` in the request's language — Go's
    /// `rctx.T` — and the task is left to finish on its own, as Go's goroutine is. A refusal of
    /// either kind is also a `file_download_rejected` event to the downloader
    /// ([`App::send_file_download_rejected_event`]).
    pub async fn run_file_will_be_downloaded(
        &self,
        ctx: &HookContext,
        info: &FileInfo,
        user_id: &str,
        download_type: FileDownloadType,
    ) -> String {
        let Some(environment) = self.hook_environment() else {
            return String::new();
        };
        let args = wire_plugin::Z_FileWillBeDownloadedArgs {
            a: ctx.boxed_wire(),
            b: Some(Box::new(file_info_to_wire(info))),
            c: user_id.to_owned(),
            d: download_type.as_str().to_owned(),
        };
        let dispatch = tokio::spawn(async move {
            for (hooks, _manifest) in
                environment.hooks_implementing(hook_id::FILE_WILL_BE_DOWNLOADED)
            {
                let returns = hooks.file_will_be_downloaded(args.clone()).await;
                if !returns.a.is_empty() {
                    return returns.a;
                }
            }
            String::new()
        });
        let reason = match tokio::time::timeout(FILE_DOWNLOAD_HOOK_TIMEOUT, dispatch).await {
            Ok(Ok(reason)) => reason,
            Ok(Err(err)) => {
                // Go's goroutine cannot panic out of `RunMultiHook` (plugin RPC failures answer
                // zero values), so there is no Go behaviour to match; a download is not refused
                // because this process failed.
                tracing::error!(error = %err, "the FileWillBeDownloaded dispatch failed");
                String::new()
            }
            Err(_elapsed) => {
                tracing::warn!(
                    file_id = %info.id,
                    user_id,
                    "FileWillBeDownloaded hook timed out, blocking download"
                );
                const TIMEOUT_ID: &str = "api.file.get_file.plugin_hook_timeout";
                match crate::i18n::loaded() {
                    Some(t) => t.translate_for_request(
                        &ctx.accept_language,
                        &self.config().default_client_locale,
                        TIMEOUT_ID,
                    ),
                    None => TIMEOUT_ID.to_owned(),
                }
            }
        };
        if !reason.is_empty() {
            self.send_file_download_rejected_event(
                info,
                user_id,
                &ctx.connection_id,
                &reason,
                download_type,
            )
            .await;
        }
        reason
    }

    /// Port of `sendFileDownloadRejectedEvent` (app/file.go:1974): `file_download_rejected` to
    /// the downloader alone — to one connection of theirs when the request named it — and
    /// nothing for a public link, which has no user.
    async fn send_file_download_rejected_event(
        &self,
        info: &FileInfo,
        user_id: &str,
        connection_id: &str,
        reason: &str,
        download_type: FileDownloadType,
    ) {
        use mm_model::websocket_message::{WEBSOCKET_EVENT_FILE_DOWNLOAD_REJECTED, WebSocketEvent};

        if user_id.is_empty() {
            tracing::debug!("Skipping websocket event for public file download rejection");
            return;
        }
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_FILE_DOWNLOAD_REJECTED,
            "",
            &info.channel_id,
            user_id,
            None,
            "",
        );
        if !connection_id.is_empty() {
            if let Some(broadcast) = message.get_broadcast() {
                let mut broadcast = broadcast.clone();
                broadcast.connection_id = connection_id.to_owned();
                message = message.set_broadcast(broadcast);
            }
        }
        for (key, value) in [
            ("file_id", info.id.as_str()),
            ("file_name", info.name.as_str()),
            ("rejection_reason", reason),
            ("channel_id", info.channel_id.as_str()),
            ("post_id", info.post_id.as_str()),
            ("download_type", download_type.as_str()),
        ] {
            message.add(key, serde_json::Value::from(value));
        }
        self.publish(message).await;
    }
    /// Port of `runPluginsHook` (app/upload.go:56) — hook 14, `FileWillBeUploaded` — after the
    /// upload is written and before its images are made. Answers whether any plugin ran, which
    /// is when Go goes back to storage for the bytes it makes the images from.
    ///
    /// `file` is what the plugins can read, and they share it: Go lends every plugin the **same**
    /// reader and the same pipe writer, so the second plugin reads what the first left, and the
    /// file's replacement is everything all of them wrote, concatenated.
    ///
    /// **`info` is mutated in place**, as Go's is: each answer is decoded into the caller's
    /// `*FileInfo`, so a plugin's fields land on the upload's own row — on a refusal too, before
    /// the refusal is looked at. When anything was written the size becomes its length and the
    /// bytes go to `info.Path` as it stands after the plugins, which a plugin may have moved.
    ///
    /// A refusal stops the iteration, sends `file_upload_rejected` to the uploader, removes the
    /// file at `info.Path` and is a 400 `app.upload.run_plugins_hook.rejected` with the file name
    /// and the reason as parameters.
    ///
    /// Go streams the replacement to `<original path>.tmp` and then moves it; this writes it once
    /// to the destination, which leaves the same file and never shows a `.tmp` to anyone.
    pub(crate) async fn run_plugins_hook(
        &self,
        ctx: &HookContext,
        info: &mut FileInfo,
        file: Vec<u8>,
    ) -> Result<bool, Box<AppError>> {
        let Some(environment) = self.hook_environment() else {
            return Ok(false);
        };
        let plugins = environment.hooks_implementing(hook_id::FILE_WILL_BE_UPLOADED);
        // "If the plugin hook has not run we can return early."
        if plugins.is_empty() {
            return Ok(false);
        }

        let reader = SharedReader {
            data: Arc::new(file),
            at: Arc::default(),
        };
        let output = SharedBuffer::default();
        let mut rejection = None;
        for (hooks, _manifest) in plugins {
            let returns = hooks
                .file_will_be_uploaded(
                    ctx.boxed_wire(),
                    Some(Box::new(file_info_to_wire(info))),
                    reader.clone(),
                    output.clone(),
                )
                .await;
            if let Some(merged) = returns.a.as_deref() {
                *info = file_info_from_wire(merged);
            }
            if !returns.b.is_empty() {
                self.send_file_upload_rejected_event(
                    info,
                    &info.creator_id,
                    &ctx.connection_id,
                    &returns.b,
                )
                .await;
                rejection = Some(returns.b);
                break;
            }
        }

        if let Some(reason) = rejection {
            if let Err(err) = self.remove_file(&info.path).await {
                tracing::warn!(error = %err, "Failed to remove file");
            }
            return Err(AppError::boxed(
                "runPluginsHook",
                "app.upload.run_plugins_hook.rejected",
                Some(std::collections::HashMap::from([
                    (
                        "Filename".to_owned(),
                        serde_json::Value::String(info.name.clone()),
                    ),
                    ("Reason".to_owned(), serde_json::Value::String(reason)),
                ])),
                String::new(),
                400,
            ));
        }

        let replacement = std::mem::take(
            &mut *output
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if !replacement.is_empty() {
            info.size = i64::try_from(replacement.len()).unwrap_or(i64::MAX);
            if let Err(err) = self.write_file(&replacement, &info.path).await {
                tracing::error!(error = %err, path = %info.path, "writing the plugins' replacement failed");
                return Err(AppError::boxed(
                    "runPluginsHook",
                    "app.upload.run_plugins_hook.move_fail",
                    None,
                    String::new(),
                    500,
                ));
            }
        }
        Ok(true)
    }

    /// Port of the `RunMultiHook` over `FileWillBeUploaded` in `DoUploadFileExpectModification`
    /// (app/file.go:1110) — the plugin API's `UploadFile` — **before** anything is written. Its
    /// rules are not [`App::run_plugins_hook`]'s:
    ///
    /// - each plugin is lent a **fresh** reader over the bytes as they stand, so the second plugin
    ///   reads the first one's replacement, and a fresh writer, so a replacement is one plugin's
    ///   output rather than everyone's concatenated;
    /// - a replacement becomes the bytes and their length the size **at once**, before the next
    ///   plugin is asked;
    /// - a refusal stops the iteration and is the 400 whose **id** is
    ///   `"File rejected by plugin. " + reason` under `DoUploadFile` — no event, and nothing to
    ///   remove, because nothing is on disk yet.
    ///
    /// `info` is merged in place as the host client decodes each answer into it. Answers the
    /// replacement bytes, when any plugin wrote some.
    pub(crate) async fn run_upload_file_hooks(
        &self,
        ctx: &HookContext,
        info: &mut FileInfo,
        data: &[u8],
    ) -> Result<Option<Vec<u8>>, Box<AppError>> {
        let Some(environment) = self.hook_environment() else {
            return Ok(None);
        };
        let plugins = environment.hooks_implementing(hook_id::FILE_WILL_BE_UPLOADED);
        if plugins.is_empty() {
            return Ok(None);
        }

        // One owned copy, shared by every reader: each is lent to a task of its own.
        let mut current = Arc::new(data.to_vec());
        let mut replaced = false;
        for (hooks, _manifest) in plugins {
            let reader = SharedReader {
                data: Arc::clone(&current),
                at: Arc::default(),
            };
            let output = SharedBuffer::default();
            let returns = hooks
                .file_will_be_uploaded(
                    ctx.boxed_wire(),
                    Some(Box::new(file_info_to_wire(info))),
                    reader,
                    output.clone(),
                )
                .await;
            if !returns.b.is_empty() {
                return Err(AppError::boxed(
                    "DoUploadFile",
                    format!("File rejected by plugin. {}", returns.b),
                    None,
                    String::new(),
                    400,
                ));
            }
            if let Some(merged) = returns.a.as_deref() {
                *info = file_info_from_wire(merged);
            }
            let written = std::mem::take(
                &mut *output
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if !written.is_empty() {
                info.size = i64::try_from(written.len()).unwrap_or(i64::MAX);
                current = Arc::new(written);
                replaced = true;
            }
        }
        Ok(replaced.then(|| Arc::unwrap_or_clone(current)))
    }

    /// Port of `sendFileUploadRejectedEvent` (app/file.go:1996): `file_upload_rejected` to the
    /// uploader, to one connection of theirs when the request named it.
    async fn send_file_upload_rejected_event(
        &self,
        info: &FileInfo,
        user_id: &str,
        connection_id: &str,
        reason: &str,
    ) {
        use mm_model::websocket_message::{WEBSOCKET_EVENT_FILE_UPLOAD_REJECTED, WebSocketEvent};

        if user_id.is_empty() {
            return;
        }
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_FILE_UPLOAD_REJECTED,
            "",
            &info.channel_id,
            user_id,
            None,
            "",
        );
        if !connection_id.is_empty() {
            if let Some(broadcast) = message.get_broadcast() {
                let mut broadcast = broadcast.clone();
                broadcast.connection_id = connection_id.to_owned();
                message = message.set_broadcast(broadcast);
            }
        }
        message.add("file_name", serde_json::Value::from(info.name.as_str()));
        message.add("rejection_reason", serde_json::Value::from(reason));
        message.add(
            "channel_id",
            serde_json::Value::from(info.channel_id.as_str()),
        );
        self.publish(message).await;
    }
    /// `PreferencesHaveChanged` (hook 42) — app/preference.go:78, the last thing
    /// `UpdatePreferences` does, after the `preferences_changed` event. **Not** fired by
    /// `DeletePreferences`: Go has no hook for a deletion.
    ///
    /// The batch is the one the caller saved, in its order.
    pub(crate) fn preferences_have_changed(&self, ctx: &HookContext, preferences: &Preferences) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let context = ctx.boxed_wire();
        let wire: Vec<wire_model::Preference> = preferences
            .iter()
            .map(|p| wire_model::Preference {
                user_id: p.user_id.clone(),
                category: p.category.clone(),
                name: p.name.clone(),
                value: p.value.clone(),
            })
            .collect();
        spawn_multi_hook(
            environment,
            hook_id::PREFERENCES_HAVE_CHANGED,
            move |hooks| {
                let args = wire_plugin::Z_PreferencesHaveChangedArgs {
                    a: context.clone(),
                    b: wire.clone(),
                };
                async move {
                    hooks.preferences_have_changed(args).await;
                }
            },
        );
    }
    /// `OnInstall` (onboarding.go:81) — the one hook Go calls on **one** plugin, through
    /// `HooksForPlugin`, rather than fanning out: the plugin onboarding just installed and
    /// enabled, with `UserId` the onboarding session's user. Awaited, where the `*HasBeen*`
    /// hooks are spawned: the caller is already its own task.
    ///
    /// A plugin that is not active is Go's `Warn` and no call; an error the plugin answers is
    /// only logged. Unlike every other entry point here, the Go host is not consulted: the only
    /// caller runs under the Rust host by construction ([`App::install_onboarding_plugins`]).
    pub(crate) async fn on_install(&self, ctx: &HookContext, user_id: String, id: &str) {
        let Some(environment) = self.plugins_environment() else {
            tracing::warn!(
                plugin_id = id,
                "Getting hooks for plugin failed: plugins are not initialized"
            );
            return;
        };
        let hooks = match environment.hooks_for_plugin(id) {
            Ok(hooks) => hooks,
            Err(err) => {
                tracing::warn!(plugin_id = id, error = %err, "Getting hooks for plugin failed");
                return;
            }
        };
        let args = wire_plugin::Z_OnInstallArgs {
            a: ctx.boxed_wire(),
            b: wire_model::OnInstallEvent { user_id },
        };
        let returns = hooks.on_install(args).await;
        if let Some(err) = mm_plugin::error::decodable_error(returns.a.as_ref()) {
            tracing::error!(plugin_id = id, error = %err.go_error(), "Plugin OnInstall hook failed");
        }
    }

    /// `GenerateSupportData` — the plugin loop at the end of `GenerateSupportPacket`
    /// (app/support_packet.go:103-126), through `RunMultiHook`: every running plugin that
    /// implements it, in the environment's order, and **none short-circuits** — the closure
    /// always answers `true`.
    ///
    /// A plugin whose manifest carries a `support_packet` prop (any value, `null` included: Go
    /// tests the key) has a checkbox in the System Console and is asked only when its id is in
    /// `plugin_packets`; a plugin without one is always asked. An error the plugin answers is a
    /// warning, and that plugin's files are dropped even when it sent some; a transport failure
    /// is Go's logged zero value — no files, no warning. Files are appended in the order the
    /// plugin sent them.
    pub(crate) async fn run_generate_support_data(
        &self,
        ctx: &HookContext,
        plugin_packets: &[String],
    ) -> (Vec<mm_model::support_packet::FileData>, Vec<String>) {
        let (mut files, mut warnings) = (Vec::new(), Vec::new());
        let Some(environment) = self.hook_environment() else {
            return (files, warnings);
        };
        for (hooks, manifest) in environment.hooks_implementing(hook_id::GENERATE_SUPPORT_DATA) {
            if !support_data_requested(&manifest, plugin_packets) {
                continue;
            }
            let returns = hooks
                .generate_support_data(wire_plugin::Z_GenerateSupportDataArgs {
                    a: ctx.boxed_wire(),
                })
                .await;
            if let Some(err) = mm_plugin::error::decodable_error(returns.b.as_ref()) {
                let text = err.go_error();
                tracing::warn!(
                    plugin = %manifest.id,
                    error = %text,
                    "Failed to generate plugin file for Support Packet"
                );
                warnings.push(text);
                continue;
            }
            files.extend(
                returns
                    .a
                    .into_iter()
                    .map(|data| mm_model::support_packet::FileData {
                        filename: data.filename,
                        body: data.body,
                    }),
            );
        }
        (files, warnings)
    }

    /// `ChannelHasBeenCreated` (hook 13) — the three creation sites: the end of `CreateChannel`
    /// (channel.go:340, not for a space), `handleCreationEvent` for a new DM (:430, **before** the
    /// `direct_added` event), and the end of `createGroupChannel` (:716). None fires when the
    /// channel already existed.
    pub(crate) fn channel_has_been_created(&self, ctx: &HookContext, channel: &Channel) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let (context, wire) = (ctx.boxed_wire(), channel_to_wire(channel));
        spawn_multi_hook(
            environment,
            hook_id::CHANNEL_HAS_BEEN_CREATED,
            move |hooks| {
                let args = wire_plugin::Z_ChannelHasBeenCreatedArgs {
                    a: context.clone(),
                    b: Some(Box::new(wire.clone())),
                };
                async move {
                    hooks.channel_has_been_created(args).await;
                }
            },
        );
    }

    /// Port of `runGuardedChannelWillBeUpdated` (guarded_hooks.go:298) — hook 52, between
    /// `UpdateChannel`'s re-read of the old channel and the store update.
    ///
    /// A refusal is `reason != ""` with the reason as a `Reason` parameter of
    /// `app.channel.update_channel.rejected_by_plugin`. A replacement is taken whole
    /// ([`channel_from_wire`]). **On a guarded channel only**, a replacement that changes `Type`
    /// is refused, naming the plugin that last replaced it — after phase A as a whole, and after
    /// each phase B answer.
    pub(crate) async fn run_guarded_channel_will_be_updated(
        &self,
        ctx: &HookContext,
        new_channel: Channel,
        old_channel: &Channel,
    ) -> Result<Channel, Box<AppError>> {
        const CALLER: &str = "UpdateChannel";
        let rejected = |reason: &str| {
            member_rejection_error(
                CALLER,
                "app.channel.update_channel.rejected_by_plugin",
                reason,
            )
        };
        let type_mutation = |plugin_id: &str| {
            AppError::boxed(
                CALLER,
                "app.channel.update_channel.plugin_type_mutation.app_error",
                Some(std::collections::HashMap::from([(
                    "PluginID".to_owned(),
                    serde_json::Value::String(plugin_id.to_owned()),
                )])),
                String::new(),
                400,
            )
        };

        if !self.plugin_host().hosted() {
            return Ok(new_channel);
        }
        let (guards, refused) = self.resolve_guards(&new_channel.id, CALLER).await;
        if let Some(err) = refused {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(new_channel);
        };

        let old_wire = channel_to_wire(old_channel);
        let mut channel = new_channel;
        let mut last_replacing = None;
        for (hooks, manifest) in environment.hooks_implementing(hook_id::CHANNEL_WILL_BE_UPDATED) {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .channel_will_be_updated(wire_plugin::Z_ChannelWillBeUpdatedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(channel_to_wire(&channel))),
                    c: Some(Box::new(old_wire.clone())),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                channel = channel_from_wire(replacement);
                last_replacing = Some(manifest.id.clone());
            }
        }
        if !guards.is_empty() {
            if let Some(plugin_id) = &last_replacing {
                if channel.channel_type != old_channel.channel_type {
                    return Err(type_mutation(plugin_id));
                }
            }
        }

        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %channel.id,
                    caller = CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(CALLER));
            };
            let (returns, rpc_err) = hooks
                .channel_will_be_updated_with_rpc_err(wire_plugin::Z_ChannelWillBeUpdatedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(channel_to_wire(&channel))),
                    c: Some(Box::new(old_wire.clone())),
                })
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, CALLER));
            }
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                channel = channel_from_wire(replacement);
                if channel.channel_type != old_channel.channel_type {
                    return Err(type_mutation(plugin_id));
                }
            }
        }
        Ok(channel)
    }

    /// Port of `runGuardedChannelWillBeRestored` (guarded_hooks.go:497) — hook 53, before the
    /// store restore. Refuse-only: there is no replacement.
    pub(crate) async fn run_guarded_channel_will_be_restored(
        &self,
        ctx: &HookContext,
        channel: &Channel,
    ) -> Result<(), Box<AppError>> {
        const CALLER: &str = "RestoreChannel";
        let rejected = |reason: &str| {
            member_rejection_error(
                CALLER,
                "app.channel.restore_channel.rejected_by_plugin",
                reason,
            )
        };

        if !self.plugin_host().hosted() {
            return Ok(());
        }
        let (guards, refused) = self.resolve_guards(&channel.id, CALLER).await;
        if let Some(err) = refused {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(());
        };
        let wire = channel_to_wire(channel);

        for (hooks, manifest) in environment.hooks_implementing(hook_id::CHANNEL_WILL_BE_RESTORED) {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .channel_will_be_restored(wire_plugin::Z_ChannelWillBeRestoredArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(wire.clone())),
                })
                .await;
            if !returns.a.is_empty() {
                return Err(rejected(&returns.a));
            }
        }

        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %channel.id,
                    caller = CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(CALLER));
            };
            let (returns, rpc_err) = hooks
                .channel_will_be_restored_with_rpc_err(wire_plugin::Z_ChannelWillBeRestoredArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(wire.clone())),
                })
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, CALLER));
            }
            if !returns.a.is_empty() {
                return Err(rejected(&returns.a));
            }
        }
        Ok(())
    }

    /// `ChannelWillBeArchived` (hook 51) — channel.go:1745, after `DeleteChannel`'s default-channel
    /// refusal and before the store write. **A plain `RunMultiHook`**, not guarded: archiving is
    /// the one channel lifecycle step guards do not claim. Stops at the first refusal.
    pub(crate) async fn run_channel_will_be_archived(
        &self,
        ctx: &HookContext,
        channel: &Channel,
    ) -> Result<(), Box<AppError>> {
        let Some(environment) = self.hook_environment() else {
            return Ok(());
        };
        let wire = channel_to_wire(channel);
        for (hooks, _manifest) in environment.hooks_implementing(hook_id::CHANNEL_WILL_BE_ARCHIVED)
        {
            let returns = hooks
                .channel_will_be_archived(wire_plugin::Z_ChannelWillBeArchivedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(wire.clone())),
                })
                .await;
            if !returns.a.is_empty() {
                return Err(member_rejection_error(
                    "DeleteChannel",
                    "app.channel.delete_channel.rejected_by_plugin",
                    &returns.a,
                ));
            }
        }
        Ok(())
    }
    /// Port of `runGuardedDraftWillBeUpserted` (guarded_hooks.go:437) — hook 55, in
    /// `UpsertDraft` after the empty-message delete and before the store upsert (which is where
    /// `PreSave` and `IsValid` run). Guards are resolved for the draft's channel as it arrived.
    ///
    /// A refusal is `reason != ""` with a `Reason` parameter of
    /// `app.draft.upsert.rejected_by_plugin`; a replacement is taken whole
    /// ([`draft_from_wire`]).
    pub(crate) async fn run_guarded_draft_will_be_upserted(
        &self,
        ctx: &HookContext,
        draft: Draft,
    ) -> Result<Draft, Box<AppError>> {
        const CALLER: &str = "UpsertDraft";
        let rejected = |reason: &str| {
            member_rejection_error(CALLER, "app.draft.upsert.rejected_by_plugin", reason)
        };

        if !self.plugin_host().hosted() {
            return Ok(draft);
        }
        let original_channel_id = draft.channel_id.clone();
        let (guards, refused) = self.resolve_guards(&original_channel_id, CALLER).await;
        if let Some(err) = refused {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(draft);
        };

        let mut draft = draft;
        for (hooks, manifest) in environment.hooks_implementing(hook_id::DRAFT_WILL_BE_UPSERTED) {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .draft_will_be_upserted(wire_plugin::Z_DraftWillBeUpsertedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(draft_to_wire(&draft))),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                draft = draft_from_wire(replacement, &draft);
            }
        }

        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %original_channel_id,
                    caller = CALLER,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(CALLER));
            };
            let (returns, rpc_err) = hooks
                .draft_will_be_upserted_with_rpc_err(wire_plugin::Z_DraftWillBeUpsertedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(draft_to_wire(&draft))),
                })
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, CALLER));
            }
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                draft = draft_from_wire(replacement, &draft);
            }
        }
        Ok(draft)
    }

    /// Port of `runGuardedScheduledPostWillBeCreated` (guarded_hooks.go:376) — hook 54, shared
    /// by `SaveScheduledPost` and `UpdateScheduledPost`. It runs **after** `IsValid` and every
    /// channel check, and nothing validates the answer: a replacement goes straight to the
    /// store.
    ///
    /// The two callers differ only in `caller` (the name the guard errors carry) and
    /// `rejection_id` — `app.scheduled_post.save.rejected_by_plugin` or
    /// `app.scheduled_post.update.rejected_by_plugin`, 400 with the reason as a `Reason`
    /// parameter. Guards are resolved for the channel as it arrived; a replacement is taken whole
    /// ([`scheduled_post_from_wire`]).
    pub(crate) async fn run_guarded_scheduled_post_will_be_created(
        &self,
        ctx: &HookContext,
        scheduled_post: ScheduledPost,
        caller: &'static str,
        rejection_id: &'static str,
    ) -> Result<ScheduledPost, Box<AppError>> {
        let rejected = |reason: &str| member_rejection_error(caller, rejection_id, reason);

        if !self.plugin_host().hosted() {
            return Ok(scheduled_post);
        }
        let original_channel_id = scheduled_post.channel_id.clone();
        let (guards, refused) = self.resolve_guards(&original_channel_id, caller).await;
        if let Some(err) = refused {
            return Err(err);
        }
        let Some(environment) = self.hook_environment() else {
            return Ok(scheduled_post);
        };

        let mut scheduled_post = scheduled_post;
        for (hooks, manifest) in
            environment.hooks_implementing(hook_id::SCHEDULED_POST_WILL_BE_CREATED)
        {
            if guards.contains(&manifest.id) {
                continue;
            }
            let returns = hooks
                .scheduled_post_will_be_created(wire_plugin::Z_ScheduledPostWillBeCreatedArgs {
                    a: ctx.boxed_wire(),
                    b: Some(Box::new(scheduled_post_to_wire(&scheduled_post))),
                })
                .await;
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                scheduled_post = scheduled_post_from_wire(replacement, &scheduled_post);
            }
        }

        for plugin_id in &guards {
            let Ok(hooks) = environment.hooks_for_plugin(plugin_id) else {
                tracing::error!(
                    error_id = "guard_plugin_inactive",
                    channel_id = %original_channel_id,
                    caller,
                    plugin_ids = ?[plugin_id],
                    "Channel guard rejected operation: claiming plugin is not active",
                );
                return Err(inactive_guard_error(caller));
            };
            let (returns, rpc_err) = hooks
                .scheduled_post_will_be_created_with_rpc_err(
                    wire_plugin::Z_ScheduledPostWillBeCreatedArgs {
                        a: ctx.boxed_wire(),
                        b: Some(Box::new(scheduled_post_to_wire(&scheduled_post))),
                    },
                )
                .await;
            if rpc_err.is_some() {
                return Err(guard_hook_failed_error(plugin_id, caller));
            }
            if !returns.b.is_empty() {
                return Err(rejected(&returns.b));
            }
            if let Some(replacement) = returns.a.as_deref() {
                scheduled_post = scheduled_post_from_wire(replacement, &scheduled_post);
            }
        }
        Ok(scheduled_post)
    }

    /// Port of `applyPostsWillBeConsumedHook` (post.go:2999) — hooks 38 and 56,
    /// `MessagesWillBeConsumed` and `MessagesWillBeConsumedWithContext`, which Go runs on the
    /// way **out** of every post read: the eleven list readers, `GetSinglePost`, and the
    /// `rpost` that `CreatePost` and `UpdatePost` answer with.
    ///
    /// Nothing runs unless some plugin implements one of the two, and nothing runs for a map
    /// with no consumable post — a burn-on-read post is left out of the slice and left alone in
    /// the map. Every plugin implementing the plain hook is asked first, then every one
    /// implementing the context-aware one; each is handed the map **as the previous answers
    /// left it**, so the second hook sees the first hook's rewrites. A replacement is taken
    /// whole ([`post_from_wire_whole`]) and only `Metadata` is carried across. Plain
    /// `RunMultiHook`s both: a plugin that fails in transport answers no posts and is skipped.
    pub(crate) async fn apply_posts_will_be_consumed_hook(
        &self,
        ctx: &HookContext,
        posts: &mut PostMap,
    ) {
        let Some(environment) = self.hook_environment() else {
            return;
        };
        let plain = environment.hooks_implementing(hook_id::MESSAGES_WILL_BE_CONSUMED);
        let with_context =
            environment.hooks_implementing(hook_id::MESSAGES_WILL_BE_CONSUMED_WITH_CONTEXT);
        if plain.is_empty() && with_context.is_empty() {
            return;
        }

        let metadata_by_id = consumable_metadata(posts);
        if metadata_by_id.is_empty() {
            return;
        }
        let mut slice = consumed_slice(posts, &metadata_by_id);

        for (hooks, _manifest) in plain {
            let returns = hooks
                .messages_will_be_consumed(wire_plugin::Z_MessagesWillBeConsumedArgs {
                    a: slice.clone(),
                })
                .await;
            apply_consumed_replacements(posts, &metadata_by_id, returns.a);
            slice = consumed_slice(posts, &metadata_by_id);
        }

        let context = ctx.boxed_wire();
        for (hooks, _manifest) in with_context {
            let returns = hooks
                .messages_will_be_consumed_with_context(
                    wire_plugin::Z_MessagesWillBeConsumedWithContextArgs {
                        a: context.clone(),
                        b: slice.clone(),
                    },
                )
                .await;
            apply_consumed_replacements(posts, &metadata_by_id, returns.a);
            slice = consumed_slice(posts, &metadata_by_id);
        }
    }

    /// [`App::apply_posts_will_be_consumed_hook`] over a list's map. A list with no map is Go's
    /// nil map: nothing to walk, nothing fired.
    pub(crate) async fn apply_post_list_will_be_consumed_hook(
        &self,
        ctx: &HookContext,
        list: &mut mm_model::post_list::PostList,
    ) {
        if let Some(posts) = list.posts.as_mut() {
            self.apply_posts_will_be_consumed_hook(ctx, posts).await;
        }
    }

    /// Port of `applyPostWillBeConsumedHook` (post.go:3057): the one-post map, and whatever the
    /// map holds under the post's id afterwards — which is always something, because a
    /// replacement lands under the id it was asked about.
    pub(crate) async fn apply_post_will_be_consumed_hook(
        &self,
        ctx: &HookContext,
        post: &mut Post,
    ) {
        let id = post.id.clone();
        let mut posts = PostMap::new();
        posts.insert(id.clone(), std::mem::take(post));
        self.apply_posts_will_be_consumed_hook(ctx, &mut posts)
            .await;
        if let Some(consumed) = posts.remove(&id) {
            *post = consumed;
        }
    }
}

#[cfg(test)]
mod consumed_tests {
    use super::*;

    fn post(id: &str, message: &str) -> Post {
        Post {
            id: id.to_owned(),
            channel_id: "channel00000000000000000000".to_owned(),
            user_id: "user000000000000000000000000".to_owned(),
            message: message.to_owned(),
            props: Some(
                [("k".to_owned(), serde_json::Value::from(1))]
                    .into_iter()
                    .collect(),
            ),
            file_ids: Some(vec!["file00000000000000000000000".to_owned()]),
            metadata: Some(PostMetadata {
                expire_at: 7,
                ..PostMetadata::default()
            }),
            ..Post::default()
        }
    }

    fn map(posts: Vec<Post>) -> PostMap {
        posts.into_iter().map(|p| (p.id.clone(), p)).collect()
    }

    #[test]
    fn a_burn_on_read_post_is_neither_sent_nor_touched() {
        let mut burning = post("b", "burning");
        burning.post_type = POST_TYPE_BURN_ON_READ.to_owned();
        let posts = map(vec![post("a", "plain"), burning]);
        let metadata = consumable_metadata(&posts);
        assert_eq!(metadata.keys().collect::<Vec<_>>(), ["a"]);
        let slice = consumed_slice(&posts, &metadata);
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].id, "a");
        assert!(slice[0].metadata.is_none(), "ForPlugin nils the metadata");
    }

    #[test]
    fn an_empty_map_has_nothing_to_consume() {
        let posts = PostMap::new();
        assert!(consumable_metadata(&posts).is_empty());
    }

    #[test]
    fn a_replacement_is_taken_whole_with_the_metadata_put_back() {
        let mut posts = map(vec![post("a", "before")]);
        let metadata = consumable_metadata(&posts);
        let replacement = wire_model::Post {
            id: "a".to_owned(),
            message: "after".to_owned(),
            ..wire_model::Post::default()
        };
        apply_consumed_replacements(&mut posts, &metadata, vec![replacement]);
        let consumed = &posts["a"];
        assert_eq!(consumed.message, "after");
        assert_eq!(
            consumed.channel_id, "",
            "taken whole: the channel was not carried"
        );
        assert_eq!(consumed.props, None, "an empty map came back nil");
        assert_eq!(consumed.file_ids, None);
        assert_eq!(
            consumed.metadata.as_ref().map(|m| m.expire_at),
            Some(7),
            "the pre-hook metadata is put back"
        );
    }

    #[test]
    fn a_replacement_under_an_unknown_id_is_ignored() {
        let mut posts = map(vec![post("a", "before")]);
        let metadata = consumable_metadata(&posts);
        let stranger = wire_model::Post {
            id: "stranger".to_owned(),
            message: "after".to_owned(),
            ..wire_model::Post::default()
        };
        apply_consumed_replacements(&mut posts, &metadata, vec![stranger]);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts["a"].message, "before");
    }

    #[test]
    fn a_burn_on_read_replacement_is_ignored_too() {
        let mut burning = post("b", "burning");
        burning.post_type = POST_TYPE_BURN_ON_READ.to_owned();
        let mut posts = map(vec![burning]);
        let metadata = consumable_metadata(&posts);
        let replacement = wire_model::Post {
            id: "b".to_owned(),
            message: "rewritten".to_owned(),
            ..wire_model::Post::default()
        };
        apply_consumed_replacements(&mut posts, &metadata, vec![replacement]);
        assert_eq!(posts["b"].message, "burning");
    }

    #[test]
    fn the_slice_is_rebuilt_from_the_replacements() {
        let mut posts = map(vec![post("a", "before"), post("b", "other")]);
        let metadata = consumable_metadata(&posts);
        apply_consumed_replacements(
            &mut posts,
            &metadata,
            vec![wire_model::Post {
                id: "a".to_owned(),
                message: "after".to_owned(),
                ..wire_model::Post::default()
            }],
        );
        let slice = consumed_slice(&posts, &metadata);
        assert_eq!(
            slice.iter().map(|p| p.message.as_str()).collect::<Vec<_>>(),
            ["after", "other"]
        );
    }

    #[test]
    fn a_whole_post_keeps_what_it_carries() {
        let wire = wire_model::Post {
            id: "a".to_owned(),
            props: wire_model::StringInterface::from([(
                "k".to_owned(),
                json_to_interface(&serde_json::Value::from(2)),
            )]),
            file_ids: vec!["f".to_owned()],
            participants: vec![wire_model::User {
                id: "u".to_owned(),
                username: "who".to_owned(),
                ..wire_model::User::default()
            }],
            ..wire_model::Post::default()
        };
        let post = post_from_wire_whole(&wire);
        assert_eq!(
            post.props.as_ref().and_then(|p| p.get("k")).cloned(),
            Some(serde_json::Value::from(2))
        );
        assert_eq!(post.file_ids.as_deref(), Some(&["f".to_owned()][..]));
        let participants = post.participants.expect("the participants came back");
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0].username, "who");
        assert_eq!(participants[0].props, None, "an empty map is nil");
    }
}

#[cfg(test)]
mod scheduled_post_wire_tests {
    use super::*;

    fn scheduled() -> ScheduledPost {
        let mut post = ScheduledPost {
            id: "sp".to_owned(),
            scheduled_at: 9,
            processed_at: 8,
            error_code: "unknown".to_owned(),
            repeat_type: "weekly".to_owned(),
            repeat_timezone: "UTC".to_owned(),
            ..ScheduledPost::default()
        };
        post.draft.message = "hello".to_owned();
        post.draft.channel_id = "c".to_owned();
        post.draft.metadata = Some(PostMetadata {
            expire_at: 7,
            ..PostMetadata::default()
        });
        post
    }

    /// Every field crosses in both directions, the embedded draft included — except `metadata`,
    /// which no wire conversion carries ([D-931]).
    #[test]
    fn a_scheduled_post_crosses_the_wire_and_back() {
        let sent = scheduled();
        let wire = scheduled_post_to_wire(&sent);
        assert_eq!(wire.draft.message, "hello");
        assert_eq!(wire.draft.metadata, None);
        assert_eq!(wire.repeat_timezone, "UTC");
        let back = scheduled_post_from_wire(&wire, &sent);
        assert_eq!(back, sent);
    }

    /// A replacement is taken whole: what the plugin left out is zero, the id included — only
    /// `metadata` comes from the value that was sent.
    #[test]
    fn a_partial_replacement_is_taken_whole() {
        let sent = scheduled();
        let mut wire = wire_model::ScheduledPost::default();
        wire.draft.message = "only this".to_owned();
        let back = scheduled_post_from_wire(&wire, &sent);
        assert_eq!(back.message, "only this");
        assert_eq!(back.id, "");
        assert_eq!(back.channel_id, "");
        assert_eq!(back.scheduled_at, 0);
        assert_eq!(back.repeat_type, "");
        assert_eq!(back.metadata, sent.metadata);
    }
}
