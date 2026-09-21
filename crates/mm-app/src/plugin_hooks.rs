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

use mm_model::channel_member::ChannelMember;
use mm_model::post::{POST_TYPE_BURN_ON_READ, Post};
use mm_model::reaction::Reaction;
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
    fn to_wire(&self) -> wire_plugin::Context {
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
fn props_to_wire(props: Option<&mm_model::utils::StringInterface>) -> wire_model::StringInterface {
    props
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), json_to_interface(v)))
        .collect()
}

/// The reverse. Go's numbers come back as `float64`, and `go_normalize_json_numbers` writes an
/// integral one the way Go's `json.Marshal` does — without it a prop that went out as `5` would
/// be stored as `5.0`.
fn props_from_wire(props: &wire_model::StringInterface) -> mm_model::utils::StringInterface {
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
fn string_map_to_wire(map: Option<&mm_model::utils::StringMap>) -> wire_model::StringMap {
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

/// A post as gob sends it, field for field.
///
/// **Not** `Metadata`: every call site passes [`Post::for_plugin`] first, which nils it
/// (`model/post.go:1355`), and no hook site here reads a replacement's metadata back — see
/// [`App::run_guarded_message_will_be_posted`].
///
/// `Participants` **is** sent now that [`user_to_wire`] exists. It is nil on every path that
/// reaches these hooks — only the thread reads fill it — so this changes no byte any plugin has
/// seen; it removes the silent gap rather than a live divergence.
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
    fn hook_environment(&self) -> Option<Arc<PluginsEnvironment>> {
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
}
