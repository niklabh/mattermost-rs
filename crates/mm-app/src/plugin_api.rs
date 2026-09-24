//! Port of `PluginAPI` (app/plugin_api.go): the server API each plugin is served, one instance per
//! plugin (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! Every method is a thin wrapper over an app function a route already uses, so the unit is the
//! method, and what it owes Go is the answer on the wire: the value, and the `*model.AppError`
//! when there is one. A method not overridden here answers Go's `API <Name> called but not
//! implemented.` through `mm_plugin`'s default.
//!
//! # What is ported
//!
//! The logging four, the nine `KV*` methods (`crate::plugin_key_value_store`), `GetServerVersion`,
//! `GetDiagnosticId` and `GetSystemInstallDate`; and the configuration and licence methods
//! (`crate::plugin_api_config`): `GetConfig`, `GetUnsanitizedConfig`, `GetPluginConfig`,
//! `SavePluginConfig`, `LoadPluginConfiguration`, `GetLicense`, `IsEnterpriseReady`,
//! `GetBundlePath`, `GetPluginID`, `GetTelemetryId` and `GetCloudLimits`.
//!
//! The four whose Go map may be empty and non-nil are served through
//! `mm_plugin::rpc::PluginApiDynamic`, because the generated structs send an empty map as nil.
//!
//! And the methods a plugin calls after activation (the model values in `crate::plugin_api_wire`):
//! the user, team, channel, member, post, thread and session reads; `HasPermissionTo`,
//! `HasPermissionToTeam` and `HasPermissionToChannel`; `CreatePost`, `UpdatePost`, `DeletePost`,
//! `SendEphemeralPost`, `UpdateEphemeralPost`, `DeleteEphemeralPost`, `AddReaction`,
//! `RemoveReaction`, `AddChannelMember`, `CreateChannel`, `GetDirectChannel` and
//! `GetGroupChannel`; `CreateBot`, `GetBot`, `GetBots`, `PatchBot`, `UpdateBotActive`,
//! `PermanentDeleteBot` and `EnsureBotUser`; and `PublishWebSocketEvent`.
//!
//! And the user, status, preference and team methods (`plugin_api/users.rs`, which says what
//! each sanitises and which shapes are not implemented): `GetUsers`, `GetUsersByIds`,
//! `GetUsersInChannel`, `GetUsersInTeam`, `SearchUsers`, `GetProfileImage`, `CreateUser`,
//! `UpdateUser`, `UpdateUserActive`, `DeleteUser`, `UpdateUserRoles`, `UpdateUserCustomStatus`,
//! `RemoveUserCustomStatus`; `GetUserStatus`, `GetUserStatusesByIds`, `UpdateUserStatus`,
//! `SetUserStatusTimedDND`; the four preference methods; and `GetTeams`, `GetTeamsForUser`,
//! `GetTeamsUnreadForUser`, `GetTeamMembers`, `GetTeamMembersForUser`, `CreateTeamMember`,
//! `CreateTeamMembers`, `DeleteTeamMember`, `UpdateTeamMemberRoles`, `GetTeamStats`,
//! `SearchTeams`, `CreateTeam`, `UpdateTeam` and `DeleteTeam`.
//!
//! And the channel, member, sidebar, post-list, reaction and emoji methods
//! (`plugin_api/channels.rs`, which says what each answers and which shapes are not
//! implemented): `GetChannelsForTeamForUser`, `GetPublicChannelsForTeam`, `SearchChannels`,
//! `UpdateChannel`, `DeleteChannel`, `GetChannelStats`, `GetChannelMembers`,
//! `GetChannelMembersByIds`, `GetChannelMembersForUser`, `UpdateChannelMemberRoles`,
//! `UpdateChannelMemberNotifications`, `PatchChannelMembersNotifications` (its refusals only),
//! `DeleteChannelMember`; the three sidebar-category methods; `GetPostsForChannel`,
//! `GetPostsSince`, `GetPostsAfter`, `GetPostsBefore`, `SearchPostsInTeam` (a `*` search only),
//! `SearchPostsInTeamForUser` and `GetReactions`; and `GetEmoji`, `GetEmojiByName`,
//! `GetEmojiList` and `GetEmojiImage`.
//! And the file, dialog and mail methods (`plugin_api/files.rs`): `UploadFile`, `GetFileInfo`,
//! `GetFileInfos`, `GetFile`, `ReadFile`, `GetFileLink`, `CopyFileInfos`,
//! `SetFileSearchableContent`, `OpenInteractiveDialog` and `SendMail`; and `PluginHTTP`, one
//! plugin's request to another's `ServeHTTP` (`plugin_api/http.rs`).
//!
//! And the session, access-token, auth-data, OAuth-app, role and group methods
//! (`plugin_api/auth.rs`, which says which are behind `checkLDAPLicense` and where `Where` is not
//! the app function's): `CreateSession`, `ExtendSessionExpiry`, `RevokeSession`,
//! `CreateUserAccessToken`, `RevokeUserAccessToken`, `UpdateUserAuth`, the four OAuth-app methods,
//! `RolesGrantPermission`, and the twenty-one group methods from `GetGroup` to
//! `DeleteGroupConstrainedMemberships`.
//!
//! And the slash-command seven (`crate::plugin_commands`): `RegisterCommand`,
//! `UnregisterCommand`, `ListPluginCommands`, `ListBuiltInCommands`, `ListCustomCommands`,
//! `ListCommands` and `ExecuteSlashCommand`.
//!
//! # A shape the REST route forwards is not implemented here
//!
//! The app functions behind these methods refuse, before any write, the shapes this server does
//! not reproduce — the REST handler forwards those to Go. A plugin call has nowhere to forward
//! to, so it answers Go's `API <Name> called but not implemented.` for that call alone (logged
//! with the reason), and the plugin sees a transport error rather than a wrong answer.
//!
//! # The request context is the empty one
//!
//! Go's plugin API holds the `request.EmptyContext` its environment was started with, so a hook
//! fired by an API write — `MessageWillBePosted` for a plugin's own `CreatePost` — is handed a
//! `plugin.Context` of six empty strings, and the session is the zero session: no user, not
//! OAuth. Every method here passes `HookContext::default()` and `Session::default()`.
//!
//! # `Where` is on the wire here
//!
//! An `*AppError`'s `Where` is `json:"-"`, so REST clients never see it; gob carries it to a
//! plugin. A port that delegated between app functions and let `Where` drift was invisible until
//! these methods — `App::get_channel_by_name_for_team_name` was one.
//!
//! # The configuration is read per call
//!
//! Go answers from its in-memory copy; this server re-reads the document each call
//! (`load_model_config`, the document plus this process's environment), as `GET /config` does,
//! so a save made through the Go server is seen on the next call.
//!
//! # An error crosses translated, without its wrapped cause
//!
//! `NewAppError` translates at construction with the server's locale (`i18n.T`), and gob carries
//! only exported fields, so the plugin receives `Id`, the translated `Message`, `DetailedError`,
//! `StatusCode` and `Where` — and not the driver error `Wrap` attached. See [`wire_app_error`].
//!
//! # Logging
//!
//! Go's `LogDebug` and friends go through `mlog.Sugar` with the plugin id attached
//! (`a.Log().Sugar(mlog.String("plugin_id", id))`), whose `argsToFields` pairs the arguments up
//! ([`log_fields`]). The pairs arrive as strings (the Go SDK formats them with `%+v` before
//! sending), but nothing on the wire enforces it, so a key that is not a string, and a dangling
//! last argument, are logged as the complaints Go logs. The record itself is a `tracing` event,
//! not an `mlog` line: this server's log format is not Go's anywhere, so what is ported is the
//! level, the message, the plugin id and the fields, in order.

use gobwire::Dynamic;
use gobwire::Interface;
use mm_model::manifest::Manifest;
use mm_model::utils::AppError;
use mm_plugin::rpc::NotImplemented;
use mm_plugin::rpc::{Answer, PluginApiDynamic};
use mm_plugin::wire::model::AppError as WireAppError;
use mm_plugin::wire::plugin::{
    Z_GetBundlePathArgs, Z_GetBundlePathReturns, Z_GetCloudLimitsArgs, Z_GetCloudLimitsReturns,
    Z_GetConfigArgs, Z_GetConfigReturns, Z_GetLicenseArgs, Z_GetLicenseReturns,
    Z_GetPluginConfigArgs, Z_GetPluginConfigReturns, Z_GetPluginIDArgs, Z_GetPluginIDReturns,
    Z_GetTelemetryIdArgs, Z_GetTelemetryIdReturns, Z_GetUnsanitizedConfigArgs,
    Z_GetUnsanitizedConfigReturns, Z_IsEnterpriseReadyArgs, Z_IsEnterpriseReadyReturns,
    Z_LoadPluginConfigurationArgsArgs, Z_LoadPluginConfigurationArgsReturns,
    Z_SavePluginConfigReturns,
};
use mm_plugin::wire::plugin::{
    Z_GetDiagnosticIdArgs, Z_GetDiagnosticIdReturns, Z_GetServerVersionArgs,
    Z_GetServerVersionReturns, Z_GetSystemInstallDateArgs, Z_GetSystemInstallDateReturns,
    Z_KVCompareAndDeleteArgs, Z_KVCompareAndDeleteReturns, Z_KVCompareAndSetArgs,
    Z_KVCompareAndSetReturns, Z_KVDeleteAllArgs, Z_KVDeleteAllReturns, Z_KVDeleteArgs,
    Z_KVDeleteReturns, Z_KVGetArgs, Z_KVGetReturns, Z_KVListArgs, Z_KVListReturns, Z_KVSetArgs,
    Z_KVSetReturns, Z_KVSetWithExpiryArgs, Z_KVSetWithExpiryReturns, Z_KVSetWithOptionsArgs,
    Z_KVSetWithOptionsReturns, Z_LogDebugArgs, Z_LogDebugReturns, Z_LogErrorArgs,
    Z_LogErrorReturns, Z_LogInfoArgs, Z_LogInfoReturns, Z_LogWarnArgs, Z_LogWarnReturns,
};

use mm_model::channel::Channel;
use mm_model::post::Post;
use mm_model::session::Session;
use mm_model::websocket_message::WebSocketEvent;
use mm_plugin::error::PluginError;
use mm_plugin::wire::plugin as api;
use mm_store::post_store::{GetPostThreadOptions, ThreadDirection};

use crate::App;
use crate::bot::EnsureBotError;
use crate::channel_create::ChannelCreate;
use crate::channel_member::{ChannelMemberOpts, MemberWrite};
use crate::plugin_api_wire::{
    bot_from_wire, bot_get_options_from_wire, bot_patch_from_wire, bot_to_wire,
    broadcast_from_wire, custom_event_name, payload_from_wire, permission_from_wire,
    post_list_for_plugin, reaction_from_wire, session_to_wire, team_to_wire,
};
use crate::plugin_hooks::{
    HookContext, channel_from_wire, channel_member_to_wire, channel_to_wire, post_from_wire_whole,
    post_to_wire, reaction_to_wire, team_member_to_wire, user_to_wire,
};
use crate::post::PrepareError;
use crate::reaction::ReactionWrite;

mod auth;
mod channels;
mod files;
mod http;
mod users;

pub use files::{file_infos_options_from_wire, open_dialog_request_from_wire, send_mail_refusal};
pub use http::{InterPluginTarget, inter_plugin_target};

/// Port of `PluginAPI` (app/plugin_api.go:24): the app, and the plugin it serves.
///
/// Go also holds a `request.CTX`, which only `KVCompareAndDelete` reads, for its logger; the
/// `tracing` span stands in for it.
pub struct AppPluginApi {
    app: App,
    id: String,
    /// The manifest the plugin was activated with. Go keeps the pointer; the environment hands
    /// this factory a borrow, so each API keeps its own copy.
    manifest: Manifest,
}

impl AppPluginApi {
    /// Port of `NewPluginAPI` (app/plugin_api.go:32).
    pub fn new(app: App, manifest: &Manifest) -> Self {
        Self {
            app,
            id: manifest.id.clone(),
            manifest: manifest.clone(),
        }
    }

    /// The plugin this API serves.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn wire(&self, err: Box<AppError>) -> Option<Box<WireAppError>> {
        Some(wire_app_error(
            err,
            &self.app.config().default_server_locale,
        ))
    }

    fn log(&self, level: tracing::Level, msg: &str, pairs: &[Option<Interface>]) {
        let (fields, complaints) = log_fields(pairs);
        for complaint in complaints {
            tracing::error!(plugin_id = %self.id, detail = %complaint.detail, "{}", complaint.message);
        }
        let fields = render_fields(&fields);
        match level {
            tracing::Level::ERROR => {
                tracing::error!(plugin_id = %self.id, fields = %fields, "{msg}")
            }
            tracing::Level::WARN => tracing::warn!(plugin_id = %self.id, fields = %fields, "{msg}"),
            tracing::Level::INFO => tracing::info!(plugin_id = %self.id, fields = %fields, "{msg}"),
            _ => tracing::debug!(plugin_id = %self.id, fields = %fields, "{msg}"),
        }
    }
}

/// A `[]byte` as gob delivered it: an empty slice was omitted on the wire, so it is Go's nil.
fn bytes(value: &[u8]) -> Option<&[u8]> {
    (!value.is_empty()).then_some(value)
}

/// A `*model.AppError` as gob carries it to the plugin: translated at construction with the
/// server's locale, as `NewAppError` does with `i18n.T`, and only its exported fields — the error
/// `Wrap` attached is unexported, so it stays behind and `DetailedError` is what it was.
pub fn wire_app_error(mut err: Box<AppError>, default_server_locale: &str) -> Box<WireAppError> {
    translate(&mut err, default_server_locale);
    Box::new(WireAppError {
        id: err.id,
        message: err.message,
        detailed_error: err.detailed_error,
        request_id: err.request_id,
        status_code: i64::from(err.status_code),
        r#where: err.where_,
        skip_translation: err.skip_translation,
    })
}

/// A `*model.AppError` a plugin made, handed on as it arrived: nothing translates it again.
pub fn wire_app_error_as_is(err: AppError) -> Box<WireAppError> {
    Box::new(WireAppError {
        id: err.id,
        message: err.message,
        detailed_error: err.detailed_error,
        request_id: err.request_id,
        status_code: i64::from(err.status_code),
        r#where: err.where_,
        skip_translation: err.skip_translation,
    })
}

/// `NewAppError`'s translation with the server's locale, which Go performs at construction.
fn translate(err: &mut AppError, default_server_locale: &str) {
    if let Some(bundle) = crate::i18n::loaded() {
        bundle.translate_app_error(bundle.server_locale(default_server_locale), err);
    }
}

/// One complaint `argsToFields` logs at error level instead of a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogComplaint {
    /// The complaint's message.
    pub message: &'static str,
    /// Its one field: `arg=<value>` or `pos=<index>`.
    pub detail: String,
}

/// Port of logr's `Sugar.argsToFields` (logr/v2 sugar.go:166) over the pairs a plugin sent.
///
/// Pairs are read two at a time: a string key and any value. A last argument with no partner
/// ends the walk with `invalid key/value pair` (`arg`); a key that is not a string drops the pair
/// with `invalid key for key/value pair` (`pos`, the key's index) and the walk goes on after its
/// value. A `logr.Field` is taken whole in Go, but none can cross gob, so that arm is not here.
pub fn log_fields(pairs: &[Option<Interface>]) -> (Vec<(String, String)>, Vec<LogComplaint>) {
    let mut fields = Vec::new();
    let mut complaints = Vec::new();
    let mut i = 0;
    while i < pairs.len() {
        if i == pairs.len() - 1 {
            complaints.push(LogComplaint {
                message: "invalid key/value pair",
                detail: format!("arg={}", render_any(pairs[i].as_ref())),
            });
            break;
        }
        match pairs[i].as_ref().filter(|key| key.name == "string") {
            Some(key) => fields.push((
                key.downcast::<String>().unwrap_or_default(),
                render_any(pairs[i + 1].as_ref()),
            )),
            None => complaints.push(LogComplaint {
                message: "invalid key for key/value pair",
                detail: format!("pos={i}"),
            }),
        }
        i += 2;
    }
    (fields, complaints)
}

/// `logr.Any`'s text for a value that crossed gob: a string as it is, the scalar kinds Go
/// registers by name as Go prints them, a nil interface as `<nil>`, and anything else by its
/// registered type name.
fn render_any(value: Option<&Interface>) -> String {
    let Some(value) = value else {
        return "<nil>".to_owned();
    };
    match value.name.as_str() {
        "string" => value.downcast::<String>().unwrap_or_default(),
        "bool" => value.downcast::<bool>().unwrap_or_default().to_string(),
        "int" | "int8" | "int16" | "int32" | "int64" => {
            value.downcast::<i64>().unwrap_or_default().to_string()
        }
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" => {
            value.downcast::<u64>().unwrap_or_default().to_string()
        }
        "float32" | "float64" => value.downcast::<f64>().unwrap_or_default().to_string(),
        other => other.to_owned(),
    }
}

/// The fields as one `key="value"` list, in the order the plugin sent them.
fn render_fields(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl mm_plugin::rpc::PluginApi for AppPluginApi {
    // -- users ------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetUser` (app/plugin_api.go:285): the store row, **unsanitised** —
    /// password hash, auth data and MFA secret included, because gob carries every exported
    /// field and Go sanitises nothing here.
    async fn get_user(
        &self,
        args: api::Z_GetUserArgs,
    ) -> Result<api::Z_GetUserReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_user(&args.a).await, |u| user_to_wire(&u));
        Ok(api::Z_GetUserReturns { a, b })
    }

    /// Port of `PluginAPI.GetUserByEmail` (app/plugin_api.go:289), unsanitised like `GetUser`.
    async fn get_user_by_email(
        &self,
        args: api::Z_GetUserByEmailArgs,
    ) -> Result<api::Z_GetUserByEmailReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_user_by_email(&args.a).await, |u| {
            user_to_wire(&u)
        });
        Ok(api::Z_GetUserByEmailReturns { a, b })
    }

    /// Port of `PluginAPI.GetUserByUsername` (app/plugin_api.go:293), unsanitised like `GetUser`.
    async fn get_user_by_username(
        &self,
        args: api::Z_GetUserByUsernameArgs,
    ) -> Result<api::Z_GetUserByUsernameReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_user_by_username(&args.a).await, |u| {
            user_to_wire(&u)
        });
        Ok(api::Z_GetUserByUsernameReturns { a, b })
    }

    /// Port of `PluginAPI.GetUsersByUsernames` (app/plugin_api.go:301):
    /// `GetUsersByUsernames(usernames, asAdmin: true, nil)`. Unlike the three single-user reads
    /// these **are** sanitised — `sanitizeProfiles` as an admin, which keeps the email and full
    /// name and drops the password, auth data and MFA secret. A name that matches nobody is
    /// simply absent.
    async fn get_users_by_usernames(
        &self,
        args: api::Z_GetUsersByUsernamesArgs,
    ) -> Result<api::Z_GetUsersByUsernamesReturns, NotImplemented> {
        let answer = match self.app.get_users_by_usernames(&args.a).await {
            Ok(users) => api::Z_GetUsersByUsernamesReturns {
                a: users
                    .into_iter()
                    .map(|mut user| {
                        self.app.sanitize_profile(&mut user, true);
                        user_to_wire(&user)
                    })
                    .collect(),
                b: None,
            },
            Err(err) => api::Z_GetUsersByUsernamesReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetSession` (app/plugin_api.go:331): the row, token and all. A miss is
    /// Go's 400, not a 404.
    async fn get_session(
        &self,
        args: api::Z_GetSessionArgs,
    ) -> Result<api::Z_GetSessionReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_session_by_id(&args.a).await, |s| {
            session_to_wire(&s)
        });
        Ok(api::Z_GetSessionReturns { a, b })
    }

    // -- teams ------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetTeam` (app/plugin_api.go:184).
    async fn get_team(
        &self,
        args: api::Z_GetTeamArgs,
    ) -> Result<api::Z_GetTeamReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_team(&args.a).await, |t| team_to_wire(&t));
        Ok(api::Z_GetTeamReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamByName` (app/plugin_api.go:193).
    async fn get_team_by_name(
        &self,
        args: api::Z_GetTeamByNameArgs,
    ) -> Result<api::Z_GetTeamByNameReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_team_by_name(&args.a).await, |t| {
            team_to_wire(&t)
        });
        Ok(api::Z_GetTeamByNameReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamMember` (app/plugin_api.go:248): team id first, then user id.
    async fn get_team_member(
        &self,
        args: api::Z_GetTeamMemberArgs,
    ) -> Result<api::Z_GetTeamMemberReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_team_member(&args.a, &args.b).await, |m| {
            team_member_to_wire(&m)
        });
        Ok(api::Z_GetTeamMemberReturns { a, b })
    }

    // -- channels ---------------------------------------------------------------------------

    /// Port of `PluginAPI.GetChannel` (app/plugin_api.go:503).
    async fn get_channel(
        &self,
        args: api::Z_GetChannelArgs,
    ) -> Result<api::Z_GetChannelReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_channel(&args.a).await, |c| channel_to_wire(&c));
        Ok(api::Z_GetChannelReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelByName` (app/plugin_api.go:559). The plugin passes the
    /// **team** first; the app function takes the name first — the swap is Go's.
    async fn get_channel_by_name(
        &self,
        args: api::Z_GetChannelByNameArgs,
    ) -> Result<api::Z_GetChannelByNameReturns, NotImplemented> {
        let result = self.app.get_channel_by_name(&args.b, &args.a, args.c).await;
        let (a, b) = self.reply(result, |c| channel_to_wire(&c));
        Ok(api::Z_GetChannelByNameReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelByNameForTeamName` (app/plugin_api.go:563): team name first
    /// from the plugin, channel name first to the app function.
    async fn get_channel_by_name_for_team_name(
        &self,
        args: api::Z_GetChannelByNameForTeamNameArgs,
    ) -> Result<api::Z_GetChannelByNameForTeamNameReturns, NotImplemented> {
        let result = self
            .app
            .get_channel_by_name_for_team_name(&args.b, &args.a, args.c)
            .await;
        let (a, b) = self.reply(result, |c| channel_to_wire(&c));
        Ok(api::Z_GetChannelByNameForTeamNameReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelMember` (app/plugin_api.go:717): channel id, then user id.
    async fn get_channel_member(
        &self,
        args: api::Z_GetChannelMemberArgs,
    ) -> Result<api::Z_GetChannelMemberReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_channel_member(&args.a, &args.b).await, |m| {
            channel_member_to_wire(&m)
        });
        Ok(api::Z_GetChannelMemberReturns { a, b })
    }

    /// Port of `PluginAPI.GetDirectChannel` (app/plugin_api.go:590): `GetOrCreateDirectChannel`,
    /// so a "get" that **creates** the channel the first time, with the hooks and events a
    /// created DM brings. The one shape this server forwards on the REST route
    /// (`RestrictDirectMessage = "team"`) is answered as not implemented.
    async fn get_direct_channel(
        &self,
        args: api::Z_GetDirectChannelArgs,
    ) -> Result<api::Z_GetDirectChannelReturns, NotImplemented> {
        let answer = match self
            .app
            .get_or_create_direct_channel(&HookContext::default(), &args.a, &args.b)
            .await
        {
            Ok(ChannelCreate::Created(channel)) => api::Z_GetDirectChannelReturns {
                a: Some(Box::new(channel_to_wire(&channel))),
                b: None,
            },
            Ok(ChannelCreate::Forward(why)) => {
                return Err(self.not_implemented("GetDirectChannel", why));
            }
            Err(err) => api::Z_GetDirectChannelReturns {
                a: None,
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetGroupChannel` (app/plugin_api.go:594): `CreateGroupChannel` with no
    /// creator, which finds the channel when it exists and creates it when it does not.
    async fn get_group_channel(
        &self,
        args: api::Z_GetGroupChannelArgs,
    ) -> Result<api::Z_GetGroupChannelReturns, NotImplemented> {
        // No creator, so never the shared-GM forward (`creator == nil` skips `ShareChannel`).
        let result = match self
            .app
            .create_group_channel(&HookContext::default(), &args.a, "")
            .await
        {
            Ok(ChannelCreate::Created(channel)) => Ok(*channel),
            Ok(ChannelCreate::Forward(why)) => {
                return Err(self.not_implemented("GetGroupChannel", why));
            }
            Err(err) => Err(err),
        };
        let (a, b) = self.reply(result, |c| channel_to_wire(&c));
        Ok(api::Z_GetGroupChannelReturns { a, b })
    }

    /// Port of `PluginAPI.CreateChannel` (app/plugin_api.go:468): `CreateChannel` **without**
    /// adding a member. With `PrivacySettings.UseAnonymousURLs` on under an Enterprise Advanced
    /// licence, an open or private channel's name is replaced by a fresh id first.
    async fn create_channel(
        &self,
        args: api::Z_CreateChannelArgs,
    ) -> Result<api::Z_CreateChannelReturns, NotImplemented> {
        let mut channel = args.a.as_deref().map(channel_from_wire).unwrap_or_default();
        let license = match self.app.license().await {
            Ok(license) => license,
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err.id, "the licence could not be read");
                None
            }
        };
        let anonymous_urls = self.app.config().use_anonymous_urls
            && mm_model::license::minimum_enterprise_advanced_license(license.as_deref());
        // "Space backing channels have system-assigned names, not user-visible URLs."
        if !channel.is_group_or_direct() && !channel.is_space() && anonymous_urls {
            channel.name = mm_model::utils::new_id();
        }
        let answer = match self
            .app
            .create_channel(&HookContext::default(), &mut channel, false)
            .await
        {
            Ok(()) => api::Z_CreateChannelReturns {
                a: Some(Box::new(channel_to_wire(&channel))),
                b: None,
            },
            Err(err) => api::Z_CreateChannelReturns {
                a: None,
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.AddChannelMember` (app/plugin_api.go:685): the channel resolved as
    /// `resolveChannel` does, then `AddChannelMember` with no requestor and no root, so the
    /// system post says the user **joined** rather than was added.
    async fn add_channel_member(
        &self,
        args: api::Z_AddChannelMemberArgs,
    ) -> Result<api::Z_AddChannelMemberReturns, NotImplemented> {
        let channel = match self.resolve_channel(&args.a).await {
            Ok(channel) => channel,
            Err(err) => {
                return Ok(api::Z_AddChannelMemberReturns {
                    a: None,
                    b: self.wire(err),
                });
            }
        };
        let answer = match self
            .app
            .add_channel_member(
                &args.b,
                &channel,
                &ChannelMemberOpts::default(),
                &HookContext::default(),
            )
            .await
        {
            Ok(MemberWrite::Done(member)) => api::Z_AddChannelMemberReturns {
                a: Some(Box::new(channel_member_to_wire(&member))),
                b: None,
            },
            Ok(MemberWrite::Forward(why)) => {
                return Err(self.not_implemented("AddChannelMember", why));
            }
            Err(err) => api::Z_AddChannelMemberReturns {
                a: None,
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    // -- posts ------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetPost` (app/plugin_api.go:956): `GetSinglePost` without deleted
    /// posts, answered `ForPlugin`.
    async fn get_post(
        &self,
        args: api::Z_GetPostArgs,
    ) -> Result<api::Z_GetPostReturns, NotImplemented> {
        let result = self
            .app
            .get_single_post(&HookContext::default(), &args.a, false)
            .await;
        let (a, b) = self.reply(result, |p| post_to_wire(&p.for_plugin()));
        Ok(api::Z_GetPostReturns { a, b })
    }

    /// Port of `PluginAPI.GetPostThread` (app/plugin_api.go:948): `GetPostThread` with a zero
    /// `GetPostsOptions` — not collapsed, no cursor, no limit, and no `ORDER BY` — for no user,
    /// answered `ForPlugin`.
    async fn get_post_thread(
        &self,
        args: api::Z_GetPostThreadArgs,
    ) -> Result<api::Z_GetPostThreadReturns, NotImplemented> {
        let options = GetPostThreadOptions {
            user_id: "",
            skip_fetch_threads: false,
            collapsed_threads: false,
            updates_only: false,
            per_page: 0,
            direction: ThreadDirection::Unset,
            from_post: "",
            from_create_at: 0,
            from_update_at: 0,
        };
        let result = self
            .app
            .get_post_thread(&HookContext::default(), &args.a, options)
            .await;
        let (a, b) = self.reply(result, |list| post_list_for_plugin(&list));
        Ok(api::Z_GetPostThreadReturns { a, b })
    }

    /// Port of `PluginAPI.CreatePost` (app/plugin_api.go:896); see
    /// [`App::create_post_from_plugin`]. A shape the REST route would forward is answered as not
    /// implemented, decided before anything is written. A nil post — which Go dereferences and
    /// panics on — is taken as the empty one.
    async fn create_post(
        &self,
        args: api::Z_CreatePostArgs,
    ) -> Result<api::Z_CreatePostReturns, NotImplemented> {
        let post = args
            .a
            .as_deref()
            .map(post_from_wire_whole)
            .unwrap_or_default();
        let result = self
            .app
            .create_post_from_plugin(&HookContext::default(), post)
            .await;
        let (a, b) = self.reply(self.served("CreatePost", result)?, |p| post_to_wire(&p));
        Ok(api::Z_CreatePostReturns { a, b })
    }

    /// Port of `PluginAPI.UpdatePost` (app/plugin_api.go:996): `UpdatePost` with
    /// `SafeUpdate: false`, answered `ForPlugin`.
    ///
    /// A post carrying `mm_blocks_actions` is answered as not implemented: Go validates it with
    /// `ValidateMmBlocksActions` and then lets the plugin **replace** the registry, and neither
    /// half is ported. Without the prop the old post's registry is kept, as Go keeps it.
    async fn update_post(
        &self,
        args: api::Z_UpdatePostArgs,
    ) -> Result<api::Z_UpdatePostReturns, NotImplemented> {
        let post = args
            .a
            .as_deref()
            .map(post_from_wire_whole)
            .unwrap_or_default();
        if post
            .get_prop(mm_model::post::POST_PROPS_MM_BLOCKS_ACTIONS)
            .is_some()
        {
            return Err(self.not_implemented(
                "UpdatePost",
                "ValidateMmBlocksActions and AllowMmBlocksActionsUpdate are not ported",
            ));
        }
        let result = self
            .app
            .update_post(&post, &Session::default(), &HookContext::default())
            .await
            .map(|(post, _)| post);
        let (a, b) = self.reply(self.served("UpdatePost", result)?, |p| {
            post_to_wire(&p.for_plugin())
        });
        Ok(api::Z_UpdatePostReturns { a, b })
    }

    /// Port of `PluginAPI.DeletePost` (app/plugin_api.go:943): deleted **by the plugin's id**,
    /// which is what `delete_by` records in the post's props.
    async fn delete_post(
        &self,
        args: api::Z_DeletePostArgs,
    ) -> Result<api::Z_DeletePostReturns, NotImplemented> {
        let result = self
            .app
            .delete_post(&args.a, &self.id, &HookContext::default())
            .await
            .map(|_| ());
        Ok(api::Z_DeletePostReturns {
            a: self
                .served("DeletePost", result)?
                .err()
                .and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.SendEphemeralPost` (app/plugin_api.go:929): one user's
    /// `ephemeral_message`, and the post back `ForPlugin`. Go's cannot fail; a store error in the
    /// prepare here is logged and answered with a nil post.
    async fn send_ephemeral_post(
        &self,
        args: api::Z_SendEphemeralPostArgs,
    ) -> Result<api::Z_SendEphemeralPostReturns, NotImplemented> {
        let post = args
            .b
            .as_deref()
            .map(post_from_wire_whole)
            .unwrap_or_default();
        let result = self
            .app
            .send_ephemeral_post(&HookContext::default(), &args.a, post)
            .await;
        Ok(api::Z_SendEphemeralPostReturns {
            a: self.ephemeral_answer("SendEphemeralPost", result)?,
        })
    }

    /// Port of `PluginAPI.UpdateEphemeralPost` (app/plugin_api.go:934): one user's
    /// `post_edited`; see [`App::update_ephemeral_post`].
    async fn update_ephemeral_post(
        &self,
        args: api::Z_UpdateEphemeralPostArgs,
    ) -> Result<api::Z_UpdateEphemeralPostReturns, NotImplemented> {
        let post = args
            .b
            .as_deref()
            .map(post_from_wire_whole)
            .unwrap_or_default();
        let result = self
            .app
            .update_ephemeral_post(&HookContext::default(), &args.a, post)
            .await;
        Ok(api::Z_UpdateEphemeralPostReturns {
            a: self.ephemeral_answer("UpdateEphemeralPost", result)?,
        })
    }

    /// Port of `PluginAPI.DeleteEphemeralPost` (app/plugin_api.go:939): user id, then post id.
    async fn delete_ephemeral_post(
        &self,
        args: api::Z_DeleteEphemeralPostArgs,
    ) -> Result<api::Z_DeleteEphemeralPostReturns, NotImplemented> {
        self.app.delete_ephemeral_post(&args.a, &args.b).await;
        Ok(api::Z_DeleteEphemeralPostReturns {})
    }

    /// Port of `PluginAPI.AddReaction` (app/plugin_api.go:917): `SaveReactionForPost`, whose
    /// checks are the REST route's minus the handler's permission gate.
    async fn add_reaction(
        &self,
        args: api::Z_AddReactionArgs,
    ) -> Result<api::Z_AddReactionReturns, NotImplemented> {
        let reaction = args
            .a
            .as_deref()
            .map(reaction_from_wire)
            .unwrap_or_default();
        let answer = match self
            .app
            .save_reaction_for_post(&reaction, &HookContext::default())
            .await
        {
            Ok(ReactionWrite::Done(saved)) => api::Z_AddReactionReturns {
                a: Some(Box::new(reaction_to_wire(&saved))),
                b: None,
            },
            Ok(ReactionWrite::Forward(_)) => {
                return Err(self.not_implemented("AddReaction", "a burn-on-read post's reaction"));
            }
            Err(err) => api::Z_AddReactionReturns {
                a: None,
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.RemoveReaction` (app/plugin_api.go:921): `DeleteReactionForPost`;
    /// removing a reaction that is not there succeeds.
    async fn remove_reaction(
        &self,
        args: api::Z_RemoveReactionArgs,
    ) -> Result<api::Z_RemoveReactionReturns, NotImplemented> {
        let reaction = args
            .a
            .as_deref()
            .map(reaction_from_wire)
            .unwrap_or_default();
        let answer = match self
            .app
            .delete_reaction_for_post(&reaction, &HookContext::default())
            .await
        {
            Ok(ReactionWrite::Done(())) => api::Z_RemoveReactionReturns { a: None },
            Ok(ReactionWrite::Forward(_)) => {
                return Err(
                    self.not_implemented("RemoveReaction", "a burn-on-read post's reaction")
                );
            }
            Err(err) => api::Z_RemoveReactionReturns { a: self.wire(err) },
        };
        Ok(answer)
    }

    // -- permissions ------------------------------------------------------------------------

    /// Port of `PluginAPI.HasPermissionTo` (app/plugin_api.go:1250). Only the permission's `Id`
    /// is read. A nil permission — Go dereferences it and panics — is no permission.
    async fn has_permission_to(
        &self,
        args: api::Z_HasPermissionToArgs,
    ) -> Result<api::Z_HasPermissionToReturns, NotImplemented> {
        let a = match args.b.as_deref() {
            Some(permission) => {
                self.app
                    .has_permission_to(&args.a, &permission_from_wire(permission))
                    .await
            }
            None => false,
        };
        Ok(api::Z_HasPermissionToReturns { a })
    }

    /// Port of `PluginAPI.HasPermissionToTeam` (app/plugin_api.go:1254).
    async fn has_permission_to_team(
        &self,
        args: api::Z_HasPermissionToTeamArgs,
    ) -> Result<api::Z_HasPermissionToTeamReturns, NotImplemented> {
        let a = match args.c.as_deref() {
            Some(permission) => {
                self.app
                    .has_permission_to_team(&args.a, &args.b, &permission_from_wire(permission))
                    .await
            }
            None => false,
        };
        Ok(api::Z_HasPermissionToTeamReturns { a })
    }

    /// Port of `PluginAPI.HasPermissionToChannel` (app/plugin_api.go:1258): the first of the
    /// app function's two answers; the second (whether the channel was found) is dropped.
    async fn has_permission_to_channel(
        &self,
        args: api::Z_HasPermissionToChannelArgs,
    ) -> Result<api::Z_HasPermissionToChannelReturns, NotImplemented> {
        let a = match args.c.as_deref() {
            Some(permission) => {
                self.app
                    .has_permission_to_channel(&args.a, &args.b, &permission_from_wire(permission))
                    .await
                    .0
            }
            None => false,
        };
        Ok(api::Z_HasPermissionToChannelReturns { a })
    }

    // -- websocket --------------------------------------------------------------------------

    /// Port of `PluginAPI.PublishWebSocketEvent` (app/plugin_api.go:1240): the event is
    /// `custom_<plugin id>_<event>`, the broadcast the plugin's **whole** — so its user, channel,
    /// team, connection and omissions decide who receives it — and the payload the data.
    ///
    /// An empty payload arrives as gob's nil map, which Go's `SetData` stores and marshals as
    /// `"data": null`; so does this.
    async fn publish_web_socket_event(
        &self,
        args: api::Z_PublishWebSocketEventArgs,
    ) -> Result<api::Z_PublishWebSocketEventReturns, NotImplemented> {
        let name = custom_event_name(&self.id, &args.a);
        let mut event = WebSocketEvent::new(name, "", "", "", None, "")
            .set_broadcast(broadcast_from_wire(args.c.as_deref()));
        event.data = (!args.b.is_empty()).then(|| payload_from_wire(&args.b));
        self.app.publish(event).await;
        Ok(api::Z_PublishWebSocketEventReturns {})
    }

    // -- users, statuses, preferences and teams (`plugin_api/users.rs`) ---------------------

    /// `PluginAPI.GetUsers`; see [`AppPluginApi::users_get_users`].
    async fn get_users(
        &self,
        args: api::Z_GetUsersArgs,
    ) -> Result<api::Z_GetUsersReturns, NotImplemented> {
        self.users_get_users(args).await
    }

    /// `PluginAPI.GetUsersByIds`; see [`AppPluginApi::users_get_users_by_ids`].
    async fn get_users_by_ids(
        &self,
        args: api::Z_GetUsersByIdsArgs,
    ) -> Result<api::Z_GetUsersByIdsReturns, NotImplemented> {
        self.users_get_users_by_ids(args).await
    }

    /// `PluginAPI.GetUsersInChannel`; see [`AppPluginApi::users_get_users_in_channel`].
    async fn get_users_in_channel(
        &self,
        args: api::Z_GetUsersInChannelArgs,
    ) -> Result<api::Z_GetUsersInChannelReturns, NotImplemented> {
        self.users_get_users_in_channel(args).await
    }

    /// `PluginAPI.GetUsersInTeam`; see [`AppPluginApi::users_get_users_in_team`].
    async fn get_users_in_team(
        &self,
        args: api::Z_GetUsersInTeamArgs,
    ) -> Result<api::Z_GetUsersInTeamReturns, NotImplemented> {
        self.users_get_users_in_team(args).await
    }

    /// `PluginAPI.SearchUsers`; see [`AppPluginApi::users_search_users`].
    async fn search_users(
        &self,
        args: api::Z_SearchUsersArgs,
    ) -> Result<api::Z_SearchUsersReturns, NotImplemented> {
        self.users_search_users(args).await
    }

    /// `PluginAPI.GetProfileImage`; see [`AppPluginApi::users_get_profile_image`].
    async fn get_profile_image(
        &self,
        args: api::Z_GetProfileImageArgs,
    ) -> Result<api::Z_GetProfileImageReturns, NotImplemented> {
        self.users_get_profile_image(args).await
    }

    /// `PluginAPI.UpdateUser`; see [`AppPluginApi::users_update_user`].
    async fn update_user(
        &self,
        args: api::Z_UpdateUserArgs,
    ) -> Result<api::Z_UpdateUserReturns, NotImplemented> {
        self.users_update_user(args).await
    }

    /// `PluginAPI.UpdateUserActive`; see [`AppPluginApi::users_update_user_active`].
    async fn update_user_active(
        &self,
        args: api::Z_UpdateUserActiveArgs,
    ) -> Result<api::Z_UpdateUserActiveReturns, NotImplemented> {
        self.users_update_user_active(args).await
    }

    /// `PluginAPI.DeleteUser`; see [`AppPluginApi::users_delete_user`].
    async fn delete_user(
        &self,
        args: api::Z_DeleteUserArgs,
    ) -> Result<api::Z_DeleteUserReturns, NotImplemented> {
        self.users_delete_user(args).await
    }

    /// `PluginAPI.UpdateUserRoles`; see [`AppPluginApi::users_update_user_roles`].
    async fn update_user_roles(
        &self,
        args: api::Z_UpdateUserRolesArgs,
    ) -> Result<api::Z_UpdateUserRolesReturns, NotImplemented> {
        self.users_update_user_roles(args).await
    }

    /// `PluginAPI.CreateUser`; see [`AppPluginApi::users_create_user`].
    async fn create_user(
        &self,
        args: api::Z_CreateUserArgs,
    ) -> Result<api::Z_CreateUserReturns, NotImplemented> {
        self.users_create_user(args).await
    }

    /// `PluginAPI.UpdateUserCustomStatus`; see [`AppPluginApi::users_update_user_custom_status`].
    async fn update_user_custom_status(
        &self,
        args: api::Z_UpdateUserCustomStatusArgs,
    ) -> Result<api::Z_UpdateUserCustomStatusReturns, NotImplemented> {
        self.users_update_user_custom_status(args).await
    }

    /// `PluginAPI.RemoveUserCustomStatus`; see [`AppPluginApi::users_remove_user_custom_status`].
    async fn remove_user_custom_status(
        &self,
        args: api::Z_RemoveUserCustomStatusArgs,
    ) -> Result<api::Z_RemoveUserCustomStatusReturns, NotImplemented> {
        self.users_remove_user_custom_status(args).await
    }

    /// `PluginAPI.GetUserStatus`; see [`AppPluginApi::users_get_user_status`].
    async fn get_user_status(
        &self,
        args: api::Z_GetUserStatusArgs,
    ) -> Result<api::Z_GetUserStatusReturns, NotImplemented> {
        self.users_get_user_status(args).await
    }

    /// `PluginAPI.GetUserStatusesByIds`; see [`AppPluginApi::users_get_user_statuses_by_ids`].
    async fn get_user_statuses_by_ids(
        &self,
        args: api::Z_GetUserStatusesByIdsArgs,
    ) -> Result<api::Z_GetUserStatusesByIdsReturns, NotImplemented> {
        self.users_get_user_statuses_by_ids(args).await
    }

    /// `PluginAPI.UpdateUserStatus`; see [`AppPluginApi::users_update_user_status`].
    async fn update_user_status(
        &self,
        args: api::Z_UpdateUserStatusArgs,
    ) -> Result<api::Z_UpdateUserStatusReturns, NotImplemented> {
        self.users_update_user_status(args).await
    }

    /// `PluginAPI.SetUserStatusTimedDND`; see [`AppPluginApi::users_set_user_status_timed_dnd`].
    async fn set_user_status_timed_dnd(
        &self,
        args: api::Z_SetUserStatusTimedDNDArgs,
    ) -> Result<api::Z_SetUserStatusTimedDNDReturns, NotImplemented> {
        self.users_set_user_status_timed_dnd(args).await
    }

    /// `PluginAPI.GetPreferencesForUser`; see [`AppPluginApi::users_get_preferences_for_user`].
    async fn get_preferences_for_user(
        &self,
        args: api::Z_GetPreferencesForUserArgs,
    ) -> Result<api::Z_GetPreferencesForUserReturns, NotImplemented> {
        self.users_get_preferences_for_user(args).await
    }

    /// `PluginAPI.GetPreferenceForUser`; see [`AppPluginApi::users_get_preference_for_user`].
    async fn get_preference_for_user(
        &self,
        args: api::Z_GetPreferenceForUserArgs,
    ) -> Result<api::Z_GetPreferenceForUserReturns, NotImplemented> {
        self.users_get_preference_for_user(args).await
    }

    /// `PluginAPI.UpdatePreferencesForUser`; see [`AppPluginApi::users_update_preferences_for_user`].
    async fn update_preferences_for_user(
        &self,
        args: api::Z_UpdatePreferencesForUserArgs,
    ) -> Result<api::Z_UpdatePreferencesForUserReturns, NotImplemented> {
        self.users_update_preferences_for_user(args).await
    }

    /// `PluginAPI.DeletePreferencesForUser`; see [`AppPluginApi::users_delete_preferences_for_user`].
    async fn delete_preferences_for_user(
        &self,
        args: api::Z_DeletePreferencesForUserArgs,
    ) -> Result<api::Z_DeletePreferencesForUserReturns, NotImplemented> {
        self.users_delete_preferences_for_user(args).await
    }

    /// `PluginAPI.GetTeamsForUser`; see [`AppPluginApi::users_get_teams_for_user`].
    async fn get_teams_for_user(
        &self,
        args: api::Z_GetTeamsForUserArgs,
    ) -> Result<api::Z_GetTeamsForUserReturns, NotImplemented> {
        self.users_get_teams_for_user(args).await
    }

    /// `PluginAPI.GetTeamsUnreadForUser`; see [`AppPluginApi::users_get_teams_unread_for_user`].
    async fn get_teams_unread_for_user(
        &self,
        args: api::Z_GetTeamsUnreadForUserArgs,
    ) -> Result<api::Z_GetTeamsUnreadForUserReturns, NotImplemented> {
        self.users_get_teams_unread_for_user(args).await
    }

    /// `PluginAPI.GetTeamMembers`; see [`AppPluginApi::users_get_team_members`].
    async fn get_team_members(
        &self,
        args: api::Z_GetTeamMembersArgs,
    ) -> Result<api::Z_GetTeamMembersReturns, NotImplemented> {
        self.users_get_team_members(args).await
    }

    /// `PluginAPI.GetTeamMembersForUser`; see [`AppPluginApi::users_get_team_members_for_user`].
    async fn get_team_members_for_user(
        &self,
        args: api::Z_GetTeamMembersForUserArgs,
    ) -> Result<api::Z_GetTeamMembersForUserReturns, NotImplemented> {
        self.users_get_team_members_for_user(args).await
    }

    /// `PluginAPI.CreateTeamMember`; see [`AppPluginApi::users_create_team_member`].
    async fn create_team_member(
        &self,
        args: api::Z_CreateTeamMemberArgs,
    ) -> Result<api::Z_CreateTeamMemberReturns, NotImplemented> {
        self.users_create_team_member(args).await
    }

    /// `PluginAPI.CreateTeamMembers`; see [`AppPluginApi::users_create_team_members`].
    async fn create_team_members(
        &self,
        args: api::Z_CreateTeamMembersArgs,
    ) -> Result<api::Z_CreateTeamMembersReturns, NotImplemented> {
        self.users_create_team_members(args).await
    }

    /// `PluginAPI.DeleteTeamMember`; see [`AppPluginApi::users_delete_team_member`].
    async fn delete_team_member(
        &self,
        args: api::Z_DeleteTeamMemberArgs,
    ) -> Result<api::Z_DeleteTeamMemberReturns, NotImplemented> {
        self.users_delete_team_member(args).await
    }

    /// `PluginAPI.UpdateTeamMemberRoles`; see [`AppPluginApi::users_update_team_member_roles`].
    async fn update_team_member_roles(
        &self,
        args: api::Z_UpdateTeamMemberRolesArgs,
    ) -> Result<api::Z_UpdateTeamMemberRolesReturns, NotImplemented> {
        self.users_update_team_member_roles(args).await
    }

    /// `PluginAPI.GetTeamStats`; see [`AppPluginApi::users_get_team_stats`].
    async fn get_team_stats(
        &self,
        args: api::Z_GetTeamStatsArgs,
    ) -> Result<api::Z_GetTeamStatsReturns, NotImplemented> {
        self.users_get_team_stats(args).await
    }

    /// `PluginAPI.SearchTeams`; see [`AppPluginApi::users_search_teams`].
    async fn search_teams(
        &self,
        args: api::Z_SearchTeamsArgs,
    ) -> Result<api::Z_SearchTeamsReturns, NotImplemented> {
        self.users_search_teams(args).await
    }

    /// `PluginAPI.CreateTeam`; see [`AppPluginApi::users_create_team`].
    async fn create_team(
        &self,
        args: api::Z_CreateTeamArgs,
    ) -> Result<api::Z_CreateTeamReturns, NotImplemented> {
        self.users_create_team(args).await
    }

    /// `PluginAPI.UpdateTeam`; see [`AppPluginApi::users_update_team`].
    async fn update_team(
        &self,
        args: api::Z_UpdateTeamArgs,
    ) -> Result<api::Z_UpdateTeamReturns, NotImplemented> {
        self.users_update_team(args).await
    }

    /// `PluginAPI.DeleteTeam`; see [`AppPluginApi::users_delete_team`].
    async fn delete_team(
        &self,
        args: api::Z_DeleteTeamArgs,
    ) -> Result<api::Z_DeleteTeamReturns, NotImplemented> {
        self.users_delete_team(args).await
    }

    /// `PluginAPI.GetTeams`; see [`AppPluginApi::users_get_teams`].
    async fn get_teams(
        &self,
        _: api::Z_GetTeamsArgs,
    ) -> Result<api::Z_GetTeamsReturns, NotImplemented> {
        self.users_get_teams().await
    }

    // -- channels, members, sidebar, post lists, reactions and emoji (`plugin_api/channels.rs`)

    /// `PluginAPI.GetChannelsForTeamForUser`; see [`AppPluginApi::channels_get_channels_for_team_for_user`].
    async fn get_channels_for_team_for_user(
        &self,
        args: api::Z_GetChannelsForTeamForUserArgs,
    ) -> Result<api::Z_GetChannelsForTeamForUserReturns, NotImplemented> {
        self.channels_get_channels_for_team_for_user(args).await
    }

    /// `PluginAPI.GetPublicChannelsForTeam`; see [`AppPluginApi::channels_get_public_channels_for_team`].
    async fn get_public_channels_for_team(
        &self,
        args: api::Z_GetPublicChannelsForTeamArgs,
    ) -> Result<api::Z_GetPublicChannelsForTeamReturns, NotImplemented> {
        self.channels_get_public_channels_for_team(args).await
    }

    /// `PluginAPI.SearchChannels`; see [`AppPluginApi::channels_search_channels`].
    async fn search_channels(
        &self,
        args: api::Z_SearchChannelsArgs,
    ) -> Result<api::Z_SearchChannelsReturns, NotImplemented> {
        self.channels_search_channels(args).await
    }

    /// `PluginAPI.UpdateChannel`; see [`AppPluginApi::channels_update_channel`].
    async fn update_channel(
        &self,
        args: api::Z_UpdateChannelArgs,
    ) -> Result<api::Z_UpdateChannelReturns, NotImplemented> {
        self.channels_update_channel(args).await
    }

    /// `PluginAPI.DeleteChannel`; see [`AppPluginApi::channels_delete_channel`].
    async fn delete_channel(
        &self,
        args: api::Z_DeleteChannelArgs,
    ) -> Result<api::Z_DeleteChannelReturns, NotImplemented> {
        self.channels_delete_channel(args).await
    }

    /// `PluginAPI.GetChannelStats`; see [`AppPluginApi::channels_get_channel_stats`].
    async fn get_channel_stats(
        &self,
        args: api::Z_GetChannelStatsArgs,
    ) -> Result<api::Z_GetChannelStatsReturns, NotImplemented> {
        self.channels_get_channel_stats(args).await
    }

    /// `PluginAPI.GetChannelMembers`; see [`AppPluginApi::channels_get_channel_members`].
    async fn get_channel_members(
        &self,
        args: api::Z_GetChannelMembersArgs,
    ) -> Result<api::Z_GetChannelMembersReturns, NotImplemented> {
        self.channels_get_channel_members(args).await
    }

    /// `PluginAPI.GetChannelMembersByIds`; see [`AppPluginApi::channels_get_channel_members_by_ids`].
    async fn get_channel_members_by_ids(
        &self,
        args: api::Z_GetChannelMembersByIdsArgs,
    ) -> Result<api::Z_GetChannelMembersByIdsReturns, NotImplemented> {
        self.channels_get_channel_members_by_ids(args).await
    }

    /// `PluginAPI.GetChannelMembersForUser`; see [`AppPluginApi::channels_get_channel_members_for_user`].
    async fn get_channel_members_for_user(
        &self,
        args: api::Z_GetChannelMembersForUserArgs,
    ) -> Result<api::Z_GetChannelMembersForUserReturns, NotImplemented> {
        self.channels_get_channel_members_for_user(args).await
    }

    /// `PluginAPI.UpdateChannelMemberRoles`; see [`AppPluginApi::channels_update_channel_member_roles`].
    async fn update_channel_member_roles(
        &self,
        args: api::Z_UpdateChannelMemberRolesArgs,
    ) -> Result<api::Z_UpdateChannelMemberRolesReturns, NotImplemented> {
        self.channels_update_channel_member_roles(args).await
    }

    /// `PluginAPI.UpdateChannelMemberNotifications`; see [`AppPluginApi::channels_update_channel_member_notifications`].
    async fn update_channel_member_notifications(
        &self,
        args: api::Z_UpdateChannelMemberNotificationsArgs,
    ) -> Result<api::Z_UpdateChannelMemberNotificationsReturns, NotImplemented> {
        self.channels_update_channel_member_notifications(args)
            .await
    }

    /// `PluginAPI.PatchChannelMembersNotifications`; see [`AppPluginApi::channels_patch_channel_members_notifications`].
    async fn patch_channel_members_notifications(
        &self,
        args: api::Z_PatchChannelMembersNotificationsArgs,
    ) -> Result<api::Z_PatchChannelMembersNotificationsReturns, NotImplemented> {
        self.channels_patch_channel_members_notifications(args)
            .await
    }

    /// `PluginAPI.DeleteChannelMember`; see [`AppPluginApi::channels_delete_channel_member`].
    async fn delete_channel_member(
        &self,
        args: api::Z_DeleteChannelMemberArgs,
    ) -> Result<api::Z_DeleteChannelMemberReturns, NotImplemented> {
        self.channels_delete_channel_member(args).await
    }

    /// `PluginAPI.GetChannelSidebarCategories`; see [`AppPluginApi::channels_get_channel_sidebar_categories`].
    async fn get_channel_sidebar_categories(
        &self,
        args: api::Z_GetChannelSidebarCategoriesArgs,
    ) -> Result<api::Z_GetChannelSidebarCategoriesReturns, NotImplemented> {
        self.channels_get_channel_sidebar_categories(args).await
    }

    /// `PluginAPI.CreateChannelSidebarCategory`; see [`AppPluginApi::channels_create_channel_sidebar_category`].
    async fn create_channel_sidebar_category(
        &self,
        args: api::Z_CreateChannelSidebarCategoryArgs,
    ) -> Result<api::Z_CreateChannelSidebarCategoryReturns, NotImplemented> {
        self.channels_create_channel_sidebar_category(args).await
    }

    /// `PluginAPI.UpdateChannelSidebarCategories`; see [`AppPluginApi::channels_update_channel_sidebar_categories`].
    async fn update_channel_sidebar_categories(
        &self,
        args: api::Z_UpdateChannelSidebarCategoriesArgs,
    ) -> Result<api::Z_UpdateChannelSidebarCategoriesReturns, NotImplemented> {
        self.channels_update_channel_sidebar_categories(args).await
    }

    /// `PluginAPI.GetPostsForChannel`; see [`AppPluginApi::channels_get_posts_for_channel`].
    async fn get_posts_for_channel(
        &self,
        args: api::Z_GetPostsForChannelArgs,
    ) -> Result<api::Z_GetPostsForChannelReturns, NotImplemented> {
        self.channels_get_posts_for_channel(args).await
    }

    /// `PluginAPI.GetPostsSince`; see [`AppPluginApi::channels_get_posts_since`].
    async fn get_posts_since(
        &self,
        args: api::Z_GetPostsSinceArgs,
    ) -> Result<api::Z_GetPostsSinceReturns, NotImplemented> {
        self.channels_get_posts_since(args).await
    }

    /// `PluginAPI.GetPostsAfter`; see [`AppPluginApi::channels_get_posts_after`].
    async fn get_posts_after(
        &self,
        args: api::Z_GetPostsAfterArgs,
    ) -> Result<api::Z_GetPostsAfterReturns, NotImplemented> {
        self.channels_get_posts_after(args).await
    }

    /// `PluginAPI.GetPostsBefore`; see [`AppPluginApi::channels_get_posts_before`].
    async fn get_posts_before(
        &self,
        args: api::Z_GetPostsBeforeArgs,
    ) -> Result<api::Z_GetPostsBeforeReturns, NotImplemented> {
        self.channels_get_posts_before(args).await
    }

    /// `PluginAPI.SearchPostsInTeam`; see [`AppPluginApi::channels_search_posts_in_team`].
    async fn search_posts_in_team(
        &self,
        args: api::Z_SearchPostsInTeamArgs,
    ) -> Result<api::Z_SearchPostsInTeamReturns, NotImplemented> {
        self.channels_search_posts_in_team(args).await
    }

    /// `PluginAPI.SearchPostsInTeamForUser`; see [`AppPluginApi::channels_search_posts_in_team_for_user`].
    async fn search_posts_in_team_for_user(
        &self,
        args: api::Z_SearchPostsInTeamForUserArgs,
    ) -> Result<api::Z_SearchPostsInTeamForUserReturns, NotImplemented> {
        self.channels_search_posts_in_team_for_user(args).await
    }

    /// `PluginAPI.GetReactions`; see [`AppPluginApi::channels_get_reactions`].
    async fn get_reactions(
        &self,
        args: api::Z_GetReactionsArgs,
    ) -> Result<api::Z_GetReactionsReturns, NotImplemented> {
        self.channels_get_reactions(args).await
    }

    /// `PluginAPI.GetEmoji`; see [`AppPluginApi::channels_get_emoji`].
    async fn get_emoji(
        &self,
        args: api::Z_GetEmojiArgs,
    ) -> Result<api::Z_GetEmojiReturns, NotImplemented> {
        self.channels_get_emoji(args).await
    }

    /// `PluginAPI.GetEmojiByName`; see [`AppPluginApi::channels_get_emoji_by_name`].
    async fn get_emoji_by_name(
        &self,
        args: api::Z_GetEmojiByNameArgs,
    ) -> Result<api::Z_GetEmojiByNameReturns, NotImplemented> {
        self.channels_get_emoji_by_name(args).await
    }

    /// `PluginAPI.GetEmojiList`; see [`AppPluginApi::channels_get_emoji_list`].
    async fn get_emoji_list(
        &self,
        args: api::Z_GetEmojiListArgs,
    ) -> Result<api::Z_GetEmojiListReturns, NotImplemented> {
        self.channels_get_emoji_list(args).await
    }

    /// `PluginAPI.GetEmojiImage`; see [`AppPluginApi::channels_get_emoji_image`].
    async fn get_emoji_image(
        &self,
        args: api::Z_GetEmojiImageArgs,
    ) -> Result<api::Z_GetEmojiImageReturns, NotImplemented> {
        self.channels_get_emoji_image(args).await
    }

    // -- files, dialogs and mail (`plugin_api/files.rs`) ------------------------------------

    /// `PluginAPI.UploadFile`; see [`AppPluginApi::files_upload_file`].
    async fn upload_file(
        &self,
        args: api::Z_UploadFileArgs,
    ) -> Result<api::Z_UploadFileReturns, NotImplemented> {
        self.files_upload_file(args).await
    }

    /// `PluginAPI.GetFileInfo`; see [`AppPluginApi::files_get_file_info`].
    async fn get_file_info(
        &self,
        args: api::Z_GetFileInfoArgs,
    ) -> Result<api::Z_GetFileInfoReturns, NotImplemented> {
        self.files_get_file_info(args).await
    }

    /// `PluginAPI.GetFileInfos`; see [`AppPluginApi::files_get_file_infos`].
    async fn get_file_infos(
        &self,
        args: api::Z_GetFileInfosArgs,
    ) -> Result<api::Z_GetFileInfosReturns, NotImplemented> {
        self.files_get_file_infos(args).await
    }

    /// `PluginAPI.GetFile`; see [`AppPluginApi::files_get_file`].
    async fn get_file(
        &self,
        args: api::Z_GetFileArgs,
    ) -> Result<api::Z_GetFileReturns, NotImplemented> {
        self.files_get_file(args).await
    }

    /// `PluginAPI.ReadFile`; see [`AppPluginApi::files_read_file`].
    async fn read_file(
        &self,
        args: api::Z_ReadFileArgs,
    ) -> Result<api::Z_ReadFileReturns, NotImplemented> {
        self.files_read_file(args).await
    }

    /// `PluginAPI.GetFileLink`; see [`AppPluginApi::files_get_file_link`].
    async fn get_file_link(
        &self,
        args: api::Z_GetFileLinkArgs,
    ) -> Result<api::Z_GetFileLinkReturns, NotImplemented> {
        self.files_get_file_link(args).await
    }

    /// `PluginAPI.CopyFileInfos`; see [`AppPluginApi::files_copy_file_infos`].
    async fn copy_file_infos(
        &self,
        args: api::Z_CopyFileInfosArgs,
    ) -> Result<api::Z_CopyFileInfosReturns, NotImplemented> {
        self.files_copy_file_infos(args).await
    }

    /// `PluginAPI.SetFileSearchableContent`; see
    /// [`AppPluginApi::files_set_file_searchable_content`].
    async fn set_file_searchable_content(
        &self,
        args: api::Z_SetFileSearchableContentArgs,
    ) -> Result<api::Z_SetFileSearchableContentReturns, NotImplemented> {
        self.files_set_file_searchable_content(args).await
    }

    /// `PluginAPI.OpenInteractiveDialog`; see [`AppPluginApi::files_open_interactive_dialog`].
    async fn open_interactive_dialog(
        &self,
        args: api::Z_OpenInteractiveDialogArgs,
    ) -> Result<api::Z_OpenInteractiveDialogReturns, NotImplemented> {
        self.files_open_interactive_dialog(args).await
    }

    /// `PluginAPI.SendMail`; see [`AppPluginApi::files_send_mail`].
    async fn send_mail(
        &self,
        args: api::Z_SendMailArgs,
    ) -> Result<api::Z_SendMailReturns, NotImplemented> {
        self.files_send_mail(args).await
    }

    // -- slash commands ---------------------------------------------------------------------

    /// Port of `PluginAPI.RegisterCommand` (app/plugin_api.go:82); see
    /// [`PluginCommandRegistry::register`](crate::plugin_commands::PluginCommandRegistry::register).
    /// A refusal crosses as `encodableError` makes it: an `ErrorString`. A nil command, which Go
    /// dereferences, is the empty one — an invalid command.
    async fn register_command(
        &self,
        args: api::Z_RegisterCommandArgs,
    ) -> Result<api::Z_RegisterCommandReturns, NotImplemented> {
        let command = args
            .a
            .as_deref()
            .map(crate::plugin_commands::command_from_wire)
            .unwrap_or_default();
        let result = self.app.plugin_commands().register(&self.id, command);
        Ok(api::Z_RegisterCommandReturns {
            a: result.err().and_then(|err| {
                mm_plugin::error::encodable_error(Some(&PluginError::Message(err.to_string())))
            }),
        })
    }

    /// Port of `PluginAPI.UnregisterCommand` (app/plugin_api.go:86): team id, then trigger. It
    /// removes **every** plugin's registration of the trigger there, and cannot fail.
    async fn unregister_command(
        &self,
        args: api::Z_UnregisterCommandArgs,
    ) -> Result<api::Z_UnregisterCommandReturns, NotImplemented> {
        self.app.plugin_commands().unregister(&args.a, &args.b);
        Ok(api::Z_UnregisterCommandReturns { a: None })
    }

    /// Port of `PluginAPI.ListPluginCommands` (app/plugin_api.go:1429): every plugin's commands
    /// for the team, first trigger wins.
    async fn list_plugin_commands(
        &self,
        args: api::Z_ListPluginCommandsArgs,
    ) -> Result<api::Z_ListPluginCommandsReturns, NotImplemented> {
        Ok(api::Z_ListPluginCommandsReturns {
            a: self.plugin_commands_for(&args.a),
            b: None,
        })
    }

    /// Port of `PluginAPI.ListBuiltInCommands` (app/plugin_api.go:1443); see
    /// [`App::list_built_in_commands`]. Not implemented under a non-English server locale.
    async fn list_built_in_commands(
        &self,
        _: api::Z_ListBuiltInCommandsArgs,
    ) -> Result<api::Z_ListBuiltInCommandsReturns, NotImplemented> {
        Ok(api::Z_ListBuiltInCommandsReturns {
            a: self.built_in_commands()?,
            b: None,
        })
    }

    /// Port of `PluginAPI.ListCustomCommands` (app/plugin_api.go:1424): the team's commands
    /// straight from the store, `EnableCommands` or not, **unsanitised** — tokens included.
    async fn list_custom_commands(
        &self,
        args: api::Z_ListCustomCommandsArgs,
    ) -> Result<api::Z_ListCustomCommandsReturns, NotImplemented> {
        let answer = match self.custom_commands(&args.a).await {
            Ok(commands) => api::Z_ListCustomCommandsReturns {
                a: commands,
                b: None,
            },
            Err(b) => api::Z_ListCustomCommandsReturns { a: Vec::new(), b },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.ListCommands` (app/plugin_api.go:1400): the plugins' commands, then the
    /// built-ins, then the team's — concatenated, so a trigger can appear twice.
    async fn list_commands(
        &self,
        args: api::Z_ListCommandsArgs,
    ) -> Result<api::Z_ListCommandsReturns, NotImplemented> {
        let mut all = self.plugin_commands_for(&args.a);
        all.extend(self.built_in_commands()?);
        let answer = match self.custom_commands(&args.a).await {
            Ok(custom) => {
                all.extend(custom);
                api::Z_ListCommandsReturns { a: all, b: None }
            }
            Err(b) => api::Z_ListCommandsReturns { a: Vec::new(), b },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.ExecuteSlashCommand` (app/plugin_api.go:91): the user must exist, the
    /// site URL is the configured one, and then `ExecuteCommand` with the API's empty context —
    /// so a plugin command's response post is made with the zero session.
    ///
    /// A trigger that reaches a custom or built-in command is answered as not implemented (no
    /// `DoCommand` and no outgoing request is ported), decided before that command runs; one that
    /// matches nothing is Go's 404. When `HandleCommandResponse` fails Go returns the response
    /// **and** the error, and so does this.
    async fn execute_slash_command(
        &self,
        args: api::Z_ExecuteSlashCommandArgs,
    ) -> Result<api::Z_ExecuteSlashCommandReturns, NotImplemented> {
        use crate::plugin_commands::{
            ExecuteOutcome, command_args_from_wire, command_response_to_wire,
        };

        let mut command_args = args
            .a
            .as_deref()
            .map(command_args_from_wire)
            .unwrap_or_default();
        let app_error = |err: Box<AppError>, translated: bool| {
            let wire = if translated {
                self.wire(err)
            } else {
                Some(wire_app_error_as_is(*err))
            };
            wire.and_then(|w| mm_plugin::error::encodable_error(Some(&PluginError::App(w))))
        };
        if let Err(err) = self.app.get_user(&command_args.user_id).await {
            return Ok(api::Z_ExecuteSlashCommandReturns {
                a: None,
                b: app_error(err, true),
            });
        }
        command_args.site_url = self.app.config().site_url.clone().unwrap_or_default();

        // `ExecuteCommand`'s own format check, before anything runs.
        let Some(trigger) = crate::command_provider::command_trigger(&command_args.command) else {
            let head = command_args
                .command
                .find(char::is_whitespace)
                .map_or(command_args.command.as_str(), |i| {
                    &command_args.command[..i]
                });
            return Ok(api::Z_ExecuteSlashCommandReturns {
                a: None,
                b: app_error(
                    AppError::boxed(
                        "command",
                        "api.command.execute_command.format.app_error",
                        Some(std::collections::HashMap::from([(
                            "Trigger".to_owned(),
                            serde_json::Value::String(mm_model::utils::go_to_lower(head)),
                        )])),
                        String::new(),
                        400,
                    ),
                    true,
                ),
            });
        };

        let answer = match self
            .app
            .execute_plugin_command(
                &HookContext::default(),
                &Session::default(),
                &mut command_args,
            )
            .await
        {
            ExecuteOutcome::Answered(Ok(response)) => api::Z_ExecuteSlashCommandReturns {
                a: Some(Box::new(command_response_to_wire(&response))),
                b: None,
            },
            ExecuteOutcome::Answered(Err(err)) => api::Z_ExecuteSlashCommandReturns {
                a: err
                    .response
                    .as_ref()
                    .map(|r| Box::new(command_response_to_wire(r))),
                b: app_error(err.error, !err.from_plugin),
            },
            ExecuteOutcome::NotPlugin => {
                match self
                    .app
                    .command_dispatch(&command_args.team_id, &command_args.user_id, &trigger)
                    .await
                {
                    Ok(crate::command_provider::CommandDispatch::NotFound(err)) | Err(err) => {
                        api::Z_ExecuteSlashCommandReturns {
                            a: None,
                            b: app_error(err, true),
                        }
                    }
                    Ok(_) => {
                        return Err(self.not_implemented(
                            "ExecuteSlashCommand",
                            "a custom or built-in command runs only in Go",
                        ));
                    }
                }
            }
        };
        Ok(answer)
    }

    // -- bots -------------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateBot` (app/plugin_api.go:1287). An empty owner becomes the
    /// plugin's id, and an owner that is itself a bot is refused (400
    /// `plugin_api.bot_cant_create_bot`) before anything is written.
    ///
    /// A bot owned by a **human** is answered as not implemented: Go then opens a DM with the
    /// owner and posts into it as the bot, which this server does not reproduce ([D-281]). The
    /// owner is read here before any write, so that refusal leaves nothing behind.
    async fn create_bot(
        &self,
        args: api::Z_CreateBotArgs,
    ) -> Result<api::Z_CreateBotReturns, NotImplemented> {
        let mut bot = args.a.as_deref().map(bot_from_wire).unwrap_or_default();
        if bot.owner_id.is_empty() {
            bot.owner_id.clone_from(&self.id);
        }
        if let Ok(owner) = self.app.get_user(&bot.owner_id).await {
            if owner.is_bot {
                return Ok(api::Z_CreateBotReturns {
                    a: None,
                    b: self.wire(AppError::boxed(
                        "CreateBot",
                        "plugin_api.bot_cant_create_bot",
                        None,
                        String::new(),
                        400,
                    )),
                });
            }
            return Err(self.not_implemented(
                "CreateBot",
                "a bot owned by a user gets a DM from its bot (D-281)",
            ));
        }
        let (a, b) = self.reply(self.app.create_bot(&bot).await, |b| bot_to_wire(&b));
        Ok(api::Z_CreateBotReturns { a, b })
    }

    /// Port of `PluginAPI.PatchBot` (app/plugin_api.go:1303). A nil patch patches nothing.
    async fn patch_bot(
        &self,
        args: api::Z_PatchBotArgs,
    ) -> Result<api::Z_PatchBotReturns, NotImplemented> {
        let patch = args
            .b
            .as_deref()
            .map(bot_patch_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.patch_bot(&args.a, &patch).await, |b| {
            bot_to_wire(&b)
        });
        Ok(api::Z_PatchBotReturns { a, b })
    }

    /// Port of `PluginAPI.GetBot` (app/plugin_api.go:1307).
    async fn get_bot(
        &self,
        args: api::Z_GetBotArgs,
    ) -> Result<api::Z_GetBotReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_bot(&args.a, args.b).await, |b| bot_to_wire(&b));
        Ok(api::Z_GetBotReturns { a, b })
    }

    /// Port of `PluginAPI.GetBots` (app/plugin_api.go:1311). Nil options are the zero options.
    async fn get_bots(
        &self,
        args: api::Z_GetBotsArgs,
    ) -> Result<api::Z_GetBotsReturns, NotImplemented> {
        let options = args
            .a
            .as_deref()
            .map(bot_get_options_from_wire)
            .unwrap_or_default();
        let answer = match self.app.get_bots(&options).await {
            Ok(bots) => api::Z_GetBotsReturns {
                a: bots.0.iter().map(bot_to_wire).collect(),
                b: None,
            },
            Err(err) => api::Z_GetBotsReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.UpdateBotActive` (app/plugin_api.go:1317): a soft delete of both rows,
    /// or its undoing, with the deactivation hook.
    async fn update_bot_active(
        &self,
        args: api::Z_UpdateBotActiveArgs,
    ) -> Result<api::Z_UpdateBotActiveReturns, NotImplemented> {
        let result = self
            .app
            .update_bot_active(&HookContext::default(), &args.a, args.b)
            .await;
        let (a, b) = self.reply(result, |b| bot_to_wire(&b));
        Ok(api::Z_UpdateBotActiveReturns { a, b })
    }

    /// Port of `PluginAPI.PermanentDeleteBot` (app/plugin_api.go:1321).
    async fn permanent_delete_bot(
        &self,
        args: api::Z_PermanentDeleteBotArgs,
    ) -> Result<api::Z_PermanentDeleteBotReturns, NotImplemented> {
        let result = self.app.permanent_delete_bot(&args.a).await;
        Ok(api::Z_PermanentDeleteBotReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.EnsureBotUser` (app/plugin_api.go:1325): the owner is **always** the
    /// plugin's id, whatever the plugin set; see [`App::ensure_bot`]. The error is an `error`,
    /// not an `*AppError`, so it crosses as `encodableError` makes it.
    async fn ensure_bot_user(
        &self,
        args: api::Z_EnsureBotUserArgs,
    ) -> Result<api::Z_EnsureBotUserReturns, NotImplemented> {
        let bot = args.a.as_deref().map(|wire| {
            let mut bot = bot_from_wire(wire);
            bot.owner_id.clone_from(&self.id);
            bot
        });
        let answer = match self.app.ensure_bot(&self.id, bot.as_ref()).await {
            Ok(id) => api::Z_EnsureBotUserReturns { a: id, b: None },
            Err(err) => api::Z_EnsureBotUserReturns {
                a: String::new(),
                b: mm_plugin::error::encodable_error(Some(&self.ensure_bot_error(err))),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.LogDebug` (app/plugin_api.go:1271).
    async fn log_debug(&self, args: Z_LogDebugArgs) -> Result<Z_LogDebugReturns, NotImplemented> {
        self.log(tracing::Level::DEBUG, &args.a, &args.b);
        Ok(Z_LogDebugReturns {})
    }

    /// Port of `PluginAPI.LogInfo` (app/plugin_api.go:1275).
    async fn log_info(&self, args: Z_LogInfoArgs) -> Result<Z_LogInfoReturns, NotImplemented> {
        self.log(tracing::Level::INFO, &args.a, &args.b);
        Ok(Z_LogInfoReturns {})
    }

    /// Port of `PluginAPI.LogError` (app/plugin_api.go:1279).
    async fn log_error(&self, args: Z_LogErrorArgs) -> Result<Z_LogErrorReturns, NotImplemented> {
        self.log(tracing::Level::ERROR, &args.a, &args.b);
        Ok(Z_LogErrorReturns {})
    }

    /// Port of `PluginAPI.LogWarn` (app/plugin_api.go:1283).
    async fn log_warn(&self, args: Z_LogWarnArgs) -> Result<Z_LogWarnReturns, NotImplemented> {
        self.log(tracing::Level::WARN, &args.a, &args.b);
        Ok(Z_LogWarnReturns {})
    }

    /// Port of `PluginAPI.KVSet` (app/plugin_api.go:1208). An empty value deletes the key.
    async fn kv_set(&self, args: Z_KVSetArgs) -> Result<Z_KVSetReturns, NotImplemented> {
        let result = self
            .app
            .set_plugin_key(&self.id, &args.a, bytes(&args.b))
            .await;
        Ok(Z_KVSetReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVSetWithExpiry` (app/plugin_api.go:1220).
    async fn kv_set_with_expiry(
        &self,
        args: Z_KVSetWithExpiryArgs,
    ) -> Result<Z_KVSetWithExpiryReturns, NotImplemented> {
        let result = self
            .app
            .set_plugin_key_with_expiry(&self.id, &args.a, bytes(&args.b), args.c)
            .await;
        Ok(Z_KVSetWithExpiryReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVSetWithOptions` (app/plugin_api.go:1204).
    async fn kv_set_with_options(
        &self,
        args: Z_KVSetWithOptionsArgs,
    ) -> Result<Z_KVSetWithOptionsReturns, NotImplemented> {
        let options = mm_model::plugin_kvset_options::PluginKVSetOptions {
            atomic: args.c.atomic,
            old_value: bytes(&args.c.old_value).map(<[u8]>::to_vec),
            expire_in_seconds: args.c.expire_in_seconds,
        };
        let answer = match self
            .app
            .set_plugin_key_with_options(&self.id, &args.a, bytes(&args.b), &options)
            .await
        {
            Ok(set) => Z_KVSetWithOptionsReturns { a: set, b: None },
            Err(e) => Z_KVSetWithOptionsReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVCompareAndSet` (app/plugin_api.go:1212).
    async fn kv_compare_and_set(
        &self,
        args: Z_KVCompareAndSetArgs,
    ) -> Result<Z_KVCompareAndSetReturns, NotImplemented> {
        let answer = match self
            .app
            .compare_and_set_plugin_key(&self.id, &args.a, bytes(&args.b), bytes(&args.c))
            .await
        {
            Ok(set) => Z_KVCompareAndSetReturns { a: set, b: None },
            Err(e) => Z_KVCompareAndSetReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVCompareAndDelete` (app/plugin_api.go:1216).
    async fn kv_compare_and_delete(
        &self,
        args: Z_KVCompareAndDeleteArgs,
    ) -> Result<Z_KVCompareAndDeleteReturns, NotImplemented> {
        let answer = match self
            .app
            .compare_and_delete_plugin_key(&self.id, &args.a, bytes(&args.b))
            .await
        {
            Ok(deleted) => Z_KVCompareAndDeleteReturns {
                a: deleted,
                b: None,
            },
            Err(e) => Z_KVCompareAndDeleteReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVGet` (app/plugin_api.go:1224). A missing key is nil, which gob
    /// sends as an omitted field.
    async fn kv_get(&self, args: Z_KVGetArgs) -> Result<Z_KVGetReturns, NotImplemented> {
        let answer = match self.app.get_plugin_key(&self.id, &args.a).await {
            Ok(value) => Z_KVGetReturns {
                a: value.unwrap_or_default(),
                b: None,
            },
            Err(e) => Z_KVGetReturns {
                a: Vec::new(),
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVDelete` (app/plugin_api.go:1228).
    async fn kv_delete(&self, args: Z_KVDeleteArgs) -> Result<Z_KVDeleteReturns, NotImplemented> {
        let result = self.app.delete_plugin_key(&self.id, &args.a).await;
        Ok(Z_KVDeleteReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVDeleteAll` (app/plugin_api.go:1232): this plugin's keys only.
    async fn kv_delete_all(
        &self,
        _: Z_KVDeleteAllArgs,
    ) -> Result<Z_KVDeleteAllReturns, NotImplemented> {
        let result = self.app.delete_all_keys_for_plugin(&self.id).await;
        Ok(Z_KVDeleteAllReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVList` (app/plugin_api.go:1236).
    async fn kv_list(&self, args: Z_KVListArgs) -> Result<Z_KVListReturns, NotImplemented> {
        let answer = match self.app.list_plugin_keys(&self.id, args.a, args.b).await {
            Ok(keys) => Z_KVListReturns { a: keys, b: None },
            Err(e) => Z_KVListReturns {
                a: Vec::new(),
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetServerVersion` (app/plugin_api.go:152): `model.CurrentVersion`.
    async fn get_server_version(
        &self,
        _: Z_GetServerVersionArgs,
    ) -> Result<Z_GetServerVersionReturns, NotImplemented> {
        Ok(Z_GetServerVersionReturns {
            a: mm_model::version::CURRENT_VERSION.to_owned(),
        })
    }

    /// Port of `PluginAPI.GetDiagnosticId` (app/plugin_api.go:160): the server id.
    async fn get_diagnostic_id(
        &self,
        _: Z_GetDiagnosticIdArgs,
    ) -> Result<Z_GetDiagnosticIdReturns, NotImplemented> {
        Ok(Z_GetDiagnosticIdReturns {
            a: self.app.server_id().await,
        })
    }

    /// Port of `PluginAPI.GetLicense` (app/plugin_api.go:143): the licence the server runs on, or
    /// nil. Go's is in memory and cannot fail to read; a store failure here is logged and nil.
    async fn get_license(
        &self,
        _: Z_GetLicenseArgs,
    ) -> Result<Z_GetLicenseReturns, NotImplemented> {
        let license = match self.app.license().await {
            Ok(license) => license,
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err.id, "the licence could not be read");
                None
            }
        };
        Ok(Z_GetLicenseReturns {
            a: license.map(|l| Box::new(crate::plugin_api_config::license_to_wire(&l))),
        })
    }

    /// Port of `PluginAPI.IsEnterpriseReady` (app/plugin_api.go:147): `strconv.ParseBool` of the
    /// build's `BuildEnterpriseReady`, false when it does not parse.
    async fn is_enterprise_ready(
        &self,
        _: Z_IsEnterpriseReadyArgs,
    ) -> Result<Z_IsEnterpriseReadyReturns, NotImplemented> {
        Ok(Z_IsEnterpriseReadyReturns {
            a: crate::config::parse_bool(mm_model::version::BUILD_ENTERPRISE_READY)
                .unwrap_or(false),
        })
    }

    /// Port of `PluginAPI.GetTelemetryId` (app/plugin_api.go:164): the server id, as
    /// `GetDiagnosticId` answers.
    async fn get_telemetry_id(
        &self,
        _: Z_GetTelemetryIdArgs,
    ) -> Result<Z_GetTelemetryIdReturns, NotImplemented> {
        Ok(Z_GetTelemetryIdReturns {
            a: self.app.server_id().await,
        })
    }

    /// Port of `PluginAPI.GetPluginID` (app/plugin_api.go:1665).
    async fn get_plugin_id(
        &self,
        _: Z_GetPluginIDArgs,
    ) -> Result<Z_GetPluginIDReturns, NotImplemented> {
        Ok(Z_GetPluginIDReturns { a: self.id.clone() })
    }

    /// Port of `PluginAPI.GetCloudLimits` (app/plugin_api.go:1564) with Go's nil cloud interface,
    /// the only one the public tree has: an empty `ProductLimits`, non-nil, so it crosses as an
    /// empty struct rather than a nil pointer.
    async fn get_cloud_limits(
        &self,
        _: Z_GetCloudLimitsArgs,
    ) -> Result<Z_GetCloudLimitsReturns, NotImplemented> {
        Ok(Z_GetCloudLimitsReturns {
            a: Some(Box::default()),
            b: None,
        })
    }

    /// Port of `PluginAPI.GetBundlePath` (app/plugin_api.go:134): `filepath.Abs` of the
    /// configured plugin directory joined with the plugin's id — relative to this process's
    /// working directory, as Go's is to its own. The one failure, an unreadable working
    /// directory, crosses as Go's `encodableError` does.
    async fn get_bundle_path(
        &self,
        _: Z_GetBundlePathArgs,
    ) -> Result<Z_GetBundlePathReturns, NotImplemented> {
        let directory = match self.app.get_sanitized_config().await {
            Ok(config) => config.plugin_settings.directory.unwrap_or_default(),
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration could not be read");
                String::new()
            }
        };
        let joined = mm_model::go_path::join(&[&directory, &self.manifest.id]);
        let answer = match crate::logs::go_abs(std::path::Path::new(&joined)) {
            Ok(path) => Z_GetBundlePathReturns {
                a: path.to_string_lossy().into_owned(),
                b: None,
            },
            Err(err) => Z_GetBundlePathReturns {
                a: String::new(),
                b: mm_plugin::error::encodable_error(Some(
                    &mm_plugin::error::PluginError::Message(err.to_string()),
                )),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.LoadPluginConfiguration` (app/plugin_api.go:50), with the host half of
    /// the RPC: the JSON of the manifest's defaults under this plugin's **unsanitised** settings
    /// (`crate::plugin_api_config::plugin_configuration`).
    async fn load_plugin_configuration(
        &self,
        _: Z_LoadPluginConfigurationArgsArgs,
    ) -> Result<Z_LoadPluginConfigurationArgsReturns, NotImplemented> {
        let config = match crate::config::load_model_config(self.app.store().config()).await {
            Ok(config) => Some(config),
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration could not be read");
                None
            }
        };
        let settings = config
            .as_ref()
            .and_then(|c| c.plugin_settings.plugins.as_ref())
            .and_then(|p| p.get(&self.id))
            .and_then(Option::as_ref);
        Ok(Z_LoadPluginConfigurationArgsReturns {
            a: crate::plugin_api_config::plugin_configuration(&self.manifest, settings),
        })
    }

    /// Port of `PluginAPI.GetSystemInstallDate` (app/plugin_api.go:156).
    async fn get_system_install_date(
        &self,
        _: Z_GetSystemInstallDateArgs,
    ) -> Result<Z_GetSystemInstallDateReturns, NotImplemented> {
        let answer = match self.app.get_system_install_date().await {
            Ok(date) => Z_GetSystemInstallDateReturns { a: date, b: None },
            Err(e) => Z_GetSystemInstallDateReturns {
                a: 0,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    // -- sessions, tokens, auth data, OAuth apps, roles and groups (`plugin_api/auth.rs`) -----

    /// `PluginAPI.CreateSession`; see [`AppPluginApi::auth_create_session`].
    async fn create_session(
        &self,
        args: api::Z_CreateSessionArgs,
    ) -> Result<api::Z_CreateSessionReturns, NotImplemented> {
        self.auth_create_session(args).await
    }

    /// `PluginAPI.ExtendSessionExpiry`; see [`AppPluginApi::auth_extend_session_expiry`].
    async fn extend_session_expiry(
        &self,
        args: api::Z_ExtendSessionExpiryArgs,
    ) -> Result<api::Z_ExtendSessionExpiryReturns, NotImplemented> {
        self.auth_extend_session_expiry(args).await
    }

    /// `PluginAPI.RevokeSession`; see [`AppPluginApi::auth_revoke_session`].
    async fn revoke_session(
        &self,
        args: api::Z_RevokeSessionArgs,
    ) -> Result<api::Z_RevokeSessionReturns, NotImplemented> {
        self.auth_revoke_session(args).await
    }

    /// `PluginAPI.CreateUserAccessToken`; see [`AppPluginApi::auth_create_user_access_token`].
    async fn create_user_access_token(
        &self,
        args: api::Z_CreateUserAccessTokenArgs,
    ) -> Result<api::Z_CreateUserAccessTokenReturns, NotImplemented> {
        self.auth_create_user_access_token(args).await
    }

    /// `PluginAPI.RevokeUserAccessToken`; see [`AppPluginApi::auth_revoke_user_access_token`].
    async fn revoke_user_access_token(
        &self,
        args: api::Z_RevokeUserAccessTokenArgs,
    ) -> Result<api::Z_RevokeUserAccessTokenReturns, NotImplemented> {
        self.auth_revoke_user_access_token(args).await
    }

    /// `PluginAPI.UpdateUserAuth`; see [`AppPluginApi::auth_update_user_auth`].
    async fn update_user_auth(
        &self,
        args: api::Z_UpdateUserAuthArgs,
    ) -> Result<api::Z_UpdateUserAuthReturns, NotImplemented> {
        self.auth_update_user_auth(args).await
    }

    /// `PluginAPI.CreateOAuthApp`; see [`AppPluginApi::auth_create_oauth_app`].
    async fn create_o_auth_app(
        &self,
        args: api::Z_CreateOAuthAppArgs,
    ) -> Result<api::Z_CreateOAuthAppReturns, NotImplemented> {
        self.auth_create_oauth_app(args).await
    }

    /// `PluginAPI.GetOAuthApp`; see [`AppPluginApi::auth_get_oauth_app`].
    async fn get_o_auth_app(
        &self,
        args: api::Z_GetOAuthAppArgs,
    ) -> Result<api::Z_GetOAuthAppReturns, NotImplemented> {
        self.auth_get_oauth_app(args).await
    }

    /// `PluginAPI.UpdateOAuthApp`; see [`AppPluginApi::auth_update_oauth_app`].
    async fn update_o_auth_app(
        &self,
        args: api::Z_UpdateOAuthAppArgs,
    ) -> Result<api::Z_UpdateOAuthAppReturns, NotImplemented> {
        self.auth_update_oauth_app(args).await
    }

    /// `PluginAPI.DeleteOAuthApp`; see [`AppPluginApi::auth_delete_oauth_app`].
    async fn delete_o_auth_app(
        &self,
        args: api::Z_DeleteOAuthAppArgs,
    ) -> Result<api::Z_DeleteOAuthAppReturns, NotImplemented> {
        self.auth_delete_oauth_app(args).await
    }

    /// `PluginAPI.RolesGrantPermission`; see [`AppPluginApi::auth_roles_grant_permission`].
    async fn roles_grant_permission(
        &self,
        args: api::Z_RolesGrantPermissionArgs,
    ) -> Result<api::Z_RolesGrantPermissionReturns, NotImplemented> {
        self.auth_roles_grant_permission(args).await
    }

    /// `PluginAPI.GetGroup`; see [`AppPluginApi::auth_get_group`].
    async fn get_group(
        &self,
        args: api::Z_GetGroupArgs,
    ) -> Result<api::Z_GetGroupReturns, NotImplemented> {
        self.auth_get_group(args).await
    }

    /// `PluginAPI.GetGroupByName`; see [`AppPluginApi::auth_get_group_by_name`].
    async fn get_group_by_name(
        &self,
        args: api::Z_GetGroupByNameArgs,
    ) -> Result<api::Z_GetGroupByNameReturns, NotImplemented> {
        self.auth_get_group_by_name(args).await
    }

    /// `PluginAPI.GetGroupMemberUsers`; see [`AppPluginApi::auth_get_group_member_users`].
    async fn get_group_member_users(
        &self,
        args: api::Z_GetGroupMemberUsersArgs,
    ) -> Result<api::Z_GetGroupMemberUsersReturns, NotImplemented> {
        self.auth_get_group_member_users(args).await
    }

    /// `PluginAPI.GetGroupsBySource`; see [`AppPluginApi::auth_get_groups_by_source`].
    async fn get_groups_by_source(
        &self,
        args: api::Z_GetGroupsBySourceArgs,
    ) -> Result<api::Z_GetGroupsBySourceReturns, NotImplemented> {
        self.auth_get_groups_by_source(args).await
    }

    /// `PluginAPI.GetGroupsForUser`; see [`AppPluginApi::auth_get_groups_for_user`].
    async fn get_groups_for_user(
        &self,
        args: api::Z_GetGroupsForUserArgs,
    ) -> Result<api::Z_GetGroupsForUserReturns, NotImplemented> {
        self.auth_get_groups_for_user(args).await
    }

    /// `PluginAPI.UpsertGroupMember`; see [`AppPluginApi::auth_upsert_group_member`].
    async fn upsert_group_member(
        &self,
        args: api::Z_UpsertGroupMemberArgs,
    ) -> Result<api::Z_UpsertGroupMemberReturns, NotImplemented> {
        self.auth_upsert_group_member(args).await
    }

    /// `PluginAPI.UpsertGroupMembers`; see [`AppPluginApi::auth_upsert_group_members`].
    async fn upsert_group_members(
        &self,
        args: api::Z_UpsertGroupMembersArgs,
    ) -> Result<api::Z_UpsertGroupMembersReturns, NotImplemented> {
        self.auth_upsert_group_members(args).await
    }

    /// `PluginAPI.GetGroupByRemoteID`; see [`AppPluginApi::auth_get_group_by_remote_id`].
    async fn get_group_by_remote_id(
        &self,
        args: api::Z_GetGroupByRemoteIDArgs,
    ) -> Result<api::Z_GetGroupByRemoteIDReturns, NotImplemented> {
        self.auth_get_group_by_remote_id(args).await
    }

    /// `PluginAPI.CreateGroup`; see [`AppPluginApi::auth_create_group`].
    async fn create_group(
        &self,
        args: api::Z_CreateGroupArgs,
    ) -> Result<api::Z_CreateGroupReturns, NotImplemented> {
        self.auth_create_group(args).await
    }

    /// `PluginAPI.UpdateGroup`; see [`AppPluginApi::auth_update_group`].
    async fn update_group(
        &self,
        args: api::Z_UpdateGroupArgs,
    ) -> Result<api::Z_UpdateGroupReturns, NotImplemented> {
        self.auth_update_group(args).await
    }

    /// `PluginAPI.DeleteGroup`; see [`AppPluginApi::auth_delete_group`].
    async fn delete_group(
        &self,
        args: api::Z_DeleteGroupArgs,
    ) -> Result<api::Z_DeleteGroupReturns, NotImplemented> {
        self.auth_delete_group(args).await
    }

    /// `PluginAPI.RestoreGroup`; see [`AppPluginApi::auth_restore_group`].
    async fn restore_group(
        &self,
        args: api::Z_RestoreGroupArgs,
    ) -> Result<api::Z_RestoreGroupReturns, NotImplemented> {
        self.auth_restore_group(args).await
    }

    /// `PluginAPI.DeleteGroupMember`; see [`AppPluginApi::auth_delete_group_member`].
    async fn delete_group_member(
        &self,
        args: api::Z_DeleteGroupMemberArgs,
    ) -> Result<api::Z_DeleteGroupMemberReturns, NotImplemented> {
        self.auth_delete_group_member(args).await
    }

    /// `PluginAPI.GetGroupSyncable`; see [`AppPluginApi::auth_get_group_syncable`].
    async fn get_group_syncable(
        &self,
        args: api::Z_GetGroupSyncableArgs,
    ) -> Result<api::Z_GetGroupSyncableReturns, NotImplemented> {
        self.auth_get_group_syncable(args).await
    }

    /// `PluginAPI.GetGroupSyncables`; see [`AppPluginApi::auth_get_group_syncables`].
    async fn get_group_syncables(
        &self,
        args: api::Z_GetGroupSyncablesArgs,
    ) -> Result<api::Z_GetGroupSyncablesReturns, NotImplemented> {
        self.auth_get_group_syncables(args).await
    }

    /// `PluginAPI.UpsertGroupSyncable`; see [`AppPluginApi::auth_upsert_group_syncable`].
    async fn upsert_group_syncable(
        &self,
        args: api::Z_UpsertGroupSyncableArgs,
    ) -> Result<api::Z_UpsertGroupSyncableReturns, NotImplemented> {
        self.auth_upsert_group_syncable(args).await
    }

    /// `PluginAPI.UpdateGroupSyncable`; see [`AppPluginApi::auth_update_group_syncable`].
    async fn update_group_syncable(
        &self,
        args: api::Z_UpdateGroupSyncableArgs,
    ) -> Result<api::Z_UpdateGroupSyncableReturns, NotImplemented> {
        self.auth_update_group_syncable(args).await
    }

    /// `PluginAPI.DeleteGroupSyncable`; see [`AppPluginApi::auth_delete_group_syncable`].
    async fn delete_group_syncable(
        &self,
        args: api::Z_DeleteGroupSyncableArgs,
    ) -> Result<api::Z_DeleteGroupSyncableReturns, NotImplemented> {
        self.auth_delete_group_syncable(args).await
    }

    /// `PluginAPI.GetGroups`; see [`AppPluginApi::auth_get_groups`].
    async fn get_groups(
        &self,
        args: api::Z_GetGroupsArgs,
    ) -> Result<api::Z_GetGroupsReturns, NotImplemented> {
        self.auth_get_groups(args).await
    }

    /// `PluginAPI.CreateDefaultSyncableMemberships`; see [`AppPluginApi::auth_create_default_syncable_memberships`].
    async fn create_default_syncable_memberships(
        &self,
        args: api::Z_CreateDefaultSyncableMembershipsArgs,
    ) -> Result<api::Z_CreateDefaultSyncableMembershipsReturns, NotImplemented> {
        self.auth_create_default_syncable_memberships(args).await
    }

    /// `PluginAPI.DeleteGroupConstrainedMemberships`; see [`AppPluginApi::auth_delete_group_constrained_memberships`].
    async fn delete_group_constrained_memberships(
        &self,
        _: api::Z_DeleteGroupConstrainedMembershipsArgs,
    ) -> Result<api::Z_DeleteGroupConstrainedMembershipsReturns, NotImplemented> {
        self.auth_delete_group_constrained_memberships().await
    }
}

impl AppPluginApi {
    /// A read's two returns: the value converted, or the error as it crosses.
    fn reply<T, W>(
        &self,
        result: Result<T, Box<AppError>>,
        convert: impl FnOnce(T) -> W,
    ) -> (Option<Box<W>>, Option<Box<WireAppError>>) {
        match result {
            Ok(value) => (Some(Box::new(convert(value))), None),
            Err(err) => (None, self.wire(err)),
        }
    }

    /// `ListPluginCommands`' answer: the team's plugin commands, first trigger wins.
    fn plugin_commands_for(&self, team_id: &str) -> Vec<mm_plugin::wire::model::Command> {
        let mut seen = std::collections::HashSet::new();
        self.app
            .plugin_commands()
            .for_team(team_id)
            .into_iter()
            .filter(|c| seen.insert(c.trigger.clone()))
            .map(|c| crate::plugin_commands::command_to_wire(&c))
            .collect()
    }

    /// `ListBuiltInCommands`' answer, or not implemented where the strings are not English.
    fn built_in_commands(&self) -> Result<Vec<mm_plugin::wire::model::Command>, NotImplemented> {
        match self.app.list_built_in_commands() {
            Some(commands) => Ok(commands
                .iter()
                .map(crate::plugin_commands::command_to_wire)
                .collect()),
            None => Err(self.not_implemented(
                "ListBuiltInCommands",
                "the built-in commands are held in English only",
            )),
        }
    }

    /// `ListCustomCommands`' answer: `Command().GetByTeam` whole. Its error is the store's, an
    /// `error` that crosses as its text; the text is this store's, not Go's `errors.Wrapf`.
    async fn custom_commands(
        &self,
        team_id: &str,
    ) -> Result<Vec<mm_plugin::wire::model::Command>, Option<Interface>> {
        use mm_store::CommandStore as _;
        match self.app.store().command().get_by_team(team_id).await {
            Ok(commands) => Ok(commands
                .iter()
                .map(crate::plugin_commands::command_to_wire)
                .collect()),
            Err(err) => Err(mm_plugin::error::encodable_error(Some(
                &PluginError::Message(format!(
                    "failed to find Commands with teamId={team_id}: {err}"
                )),
            ))),
        }
    }

    /// Go's `API <Name> called but not implemented.` for a call whose shape this server does not
    /// reproduce — the same answer the REST route turns into a forward. Every such refusal is
    /// decided before anything is written.
    fn not_implemented(&self, method: &'static str, why: &str) -> NotImplemented {
        tracing::warn!(plugin_id = %self.id, method, why, "a plugin API call this server does not reproduce");
        NotImplemented
    }

    /// A write's outcome with its unreproducible shape answered as not implemented.
    fn served<T>(
        &self,
        method: &'static str,
        result: Result<T, PrepareError>,
    ) -> Result<Result<T, Box<AppError>>, NotImplemented> {
        match result {
            Ok(value) => Ok(Ok(value)),
            Err(PrepareError::App(err)) => Ok(Err(err)),
            Err(PrepareError::Unreproducible(why)) => Err(self.not_implemented(method, why)),
        }
    }

    /// The post `SendEphemeralPost` and `UpdateEphemeralPost` answer with, `ForPlugin`. Go's
    /// cannot fail, so an error here is logged and answered as a nil post.
    fn ephemeral_answer(
        &self,
        method: &'static str,
        result: Result<Post, PrepareError>,
    ) -> Result<Option<Box<mm_plugin::wire::model::Post>>, NotImplemented> {
        Ok(match self.served(method, result)? {
            Ok(post) => Some(Box::new(post_to_wire(&post.for_plugin()))),
            Err(err) => {
                tracing::error!(plugin_id = %self.id, method, error = %err, "the ephemeral post could not be prepared");
                None
            }
        })
    }

    /// Port of `PluginAPI.resolveChannel` (app/plugin_api.go:536): `GetChannel`, and on its 404
    /// only, the same id as a space channel — which the generic read excludes. A space lookup
    /// that fails any other way is that failure; a second 404 is the first one.
    async fn resolve_channel(&self, channel_id: &str) -> Result<Channel, Box<AppError>> {
        let err = match self.app.get_channel(channel_id).await {
            Ok(channel) => return Ok(channel),
            Err(err) => err,
        };
        if err.status_code != 404 {
            return Err(err);
        }
        match self
            .app
            .get_channel_of_type(channel_id, mm_model::channel::CHANNEL_TYPE_SPACE)
            .await
        {
            Ok(space) => Ok(space),
            Err(space_err) if space_err.status_code != 404 => Err(space_err),
            Err(_) => Err(err),
        }
    }

    /// An `EnsureBot` failure as the plugin receives it. An app error inside `fmt.Errorf` is
    /// rendered with its **translated** message, as Go's `Error()` renders it — which is why the
    /// text is made here, where the server locale is known.
    fn ensure_bot_error(&self, err: EnsureBotError) -> PluginError {
        match err {
            EnsureBotError::Message(text) => PluginError::Message(text),
            EnsureBotError::App(err) => PluginError::App(wire_app_error(
                err,
                &self.app.config().default_server_locale,
            )),
            EnsureBotError::Wrapped(prefix, mut err) => {
                translate(&mut err, &self.app.config().default_server_locale);
                PluginError::Message(format!("{prefix}: {err}"))
            }
        }
    }

    /// `GetConfig` or `GetUnsanitizedConfig` as the dynamic `Z_<Method>Returns` named `name`.
    /// Go's answer cannot fail; a document this server cannot read, or cannot put into gob's
    /// shape, is logged and answered with a nil config.
    async fn config_answer(&self, name: &str, sanitized: bool) -> Dynamic {
        let config = if sanitized {
            self.app.get_sanitized_config().await
        } else {
            crate::config::load_model_config(self.app.store().config()).await
        };
        let json = match config.map(|c| serde_json::to_value(&c)) {
            Ok(Ok(json)) => Some(json),
            Ok(Err(err)) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration did not serialise");
                None
            }
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration could not be read");
                None
            }
        };
        let fallback = || crate::plugin_api_config::config_returns(name, None);
        match crate::plugin_api_config::config_returns(name, json.as_ref()) {
            Ok(answer) => answer,
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration has no gob form");
                fallback().unwrap_or_default()
            }
        }
    }
}

impl PluginApiDynamic for AppPluginApi {
    /// Port of `PluginAPI.GetConfig` (app/plugin_api.go:105): `App.GetSanitizedConfig`.
    async fn get_config_dynamic(
        &self,
        _: Z_GetConfigArgs,
    ) -> Result<Answer<Z_GetConfigReturns>, NotImplemented> {
        Ok(Answer::Dynamic(
            self.config_answer("Z_GetConfigReturns", true).await,
        ))
    }

    /// Port of `PluginAPI.GetUnsanitizedConfig` (app/plugin_api.go:110): the configuration with
    /// its secrets.
    async fn get_unsanitized_config_dynamic(
        &self,
        _: Z_GetUnsanitizedConfigArgs,
    ) -> Result<Answer<Z_GetUnsanitizedConfigReturns>, NotImplemented> {
        Ok(Answer::Dynamic(
            self.config_answer("Z_GetUnsanitizedConfigReturns", false)
                .await,
        ))
    }

    /// Port of `PluginAPI.GetPluginConfig` (app/plugin_api.go:119): this plugin's entry in the
    /// **sanitised** configuration, keyed by the manifest's id — so a secret setting reads
    /// `FakeSetting` — or an empty map when it has none.
    async fn get_plugin_config_dynamic(
        &self,
        _: Z_GetPluginConfigArgs,
    ) -> Result<Answer<Z_GetPluginConfigReturns>, NotImplemented> {
        let config = match self.app.get_sanitized_config().await {
            Ok(config) => Some(config),
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err, "the configuration could not be read");
                None
            }
        };
        let plugins = config
            .as_ref()
            .and_then(|c| c.plugin_settings.plugins.as_ref());
        Ok(Answer::Dynamic(
            crate::plugin_api_config::plugin_config_returns(plugins, &self.manifest.id),
        ))
    }

    /// Port of `PluginAPI.SavePluginConfig` (app/plugin_api.go:127), keyed by the manifest's id;
    /// see [`App::save_plugin_config`](crate::App::save_plugin_config).
    async fn save_plugin_config_dynamic(
        &self,
        args: &Dynamic,
    ) -> Result<Z_SavePluginConfigReturns, NotImplemented> {
        let result = match crate::plugin_api_config::plugin_config_from_args(args) {
            Ok(settings) => {
                self.app
                    .save_plugin_config(&self.manifest.id, settings)
                    .await
            }
            // Go's `SaveConfig` fails to persist a value `json.Marshal` refuses.
            Err(err) => Err(AppError::boxed(
                "saveConfig",
                "app.save_config.app_error",
                None,
                "",
                500,
            ))
            .inspect_err(
                |_| tracing::warn!(plugin_id = %self.id, error = %err, "SavePluginConfig failed"),
            ),
        };
        Ok(Z_SavePluginConfigReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }
}

impl mm_plugin::rpc::PluginApiStreams for AppPluginApi {}
impl mm_plugin::rpc::PluginApiHttp for AppPluginApi {
    /// `PluginAPI.PluginHTTP`; see [`AppPluginApi::http_plugin_http`].
    async fn plugin_http(
        &self,
        request: Option<Box<mm_plugin::wire::plugin::HTTPRequestSubset>>,
        body: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    ) -> Result<mm_plugin::rpc::HttpResponse, NotImplemented> {
        self.http_plugin_http(request, body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Option<Interface> {
        Some(Interface::string(text))
    }

    #[test]
    fn pairs_become_fields_in_order() {
        let (fields, complaints) = log_fields(&[s("a"), s("1"), s("b"), s("2")]);
        assert_eq!(
            fields,
            vec![("a".into(), "1".into()), ("b".into(), "2".into())]
        );
        assert!(complaints.is_empty());
        assert_eq!(log_fields(&[]), (vec![], vec![]));
    }

    /// The last argument alone ends the walk; the pairs before it are kept.
    #[test]
    fn a_dangling_argument_is_a_complaint_not_a_field() {
        let (fields, complaints) = log_fields(&[s("a"), s("1"), s("dangling")]);
        assert_eq!(fields, vec![("a".into(), "1".into())]);
        assert_eq!(
            complaints,
            vec![LogComplaint {
                message: "invalid key/value pair",
                detail: "arg=dangling".into(),
            }]
        );
    }

    /// A key that is not a string drops its pair and names the key's index; the walk goes on.
    #[test]
    fn a_key_that_is_not_a_string_is_skipped_with_its_value() {
        let (fields, complaints) = log_fields(&[
            Some(Interface::int(7)),
            s("lost"),
            s("b"),
            None,
            None,
            s("x"),
        ]);
        assert_eq!(fields, vec![("b".into(), "<nil>".into())]);
        assert_eq!(
            complaints,
            vec![
                LogComplaint {
                    message: "invalid key for key/value pair",
                    detail: "pos=0".into(),
                },
                LogComplaint {
                    message: "invalid key for key/value pair",
                    detail: "pos=4".into(),
                },
            ]
        );
        let (fields, _) = log_fields(&[
            s("n"),
            Some(Interface::int(-3)),
            s("t"),
            Some(Interface::bool(true)),
        ]);
        assert_eq!(
            fields,
            vec![("n".into(), "-3".into()), ("t".into(), "true".into())]
        );
    }

    #[test]
    fn an_empty_slice_is_go_nil() {
        assert_eq!(bytes(b""), None);
        assert_eq!(bytes(b"x"), Some(&b"x"[..]));
    }

    /// Only exported fields cross: the wrapped cause is not `DetailedError`, and the status is
    /// widened, not reinterpreted.
    #[test]
    fn an_app_error_crosses_without_its_wrapped_cause() {
        let err = Box::new(
            AppError::new(
                "ListPluginKeys",
                "app.plugin_store.list.app_error",
                None,
                "d",
                500,
            )
            .wrap(std::io::Error::other("driver")),
        );
        let wire = wire_app_error(err, "en");
        assert_eq!(wire.id, "app.plugin_store.list.app_error");
        assert_eq!(wire.detailed_error, "d");
        assert_eq!(wire.status_code, 500);
        assert_eq!(wire.r#where, "ListPluginKeys");
        assert!(!wire.skip_translation);
    }

    fn unreachable_api() -> AppPluginApi {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let app = crate::App::new(mm_store::SqlStore::from_pool(pool));
        let manifest = Manifest {
            id: "com.example.Core".into(),
            ..Manifest::default()
        };
        AppPluginApi::new(app, &manifest)
    }

    /// A nil permission, which Go would dereference, is no permission — and no store is asked.
    #[tokio::test]
    async fn a_nil_permission_is_denied_without_a_lookup() {
        use mm_plugin::rpc::PluginApi as _;
        let api = unreachable_api();
        let to = api
            .has_permission_to(api::Z_HasPermissionToArgs {
                a: "u".into(),
                b: None,
            })
            .await
            .expect("served");
        assert!(!to.a);
        let team = api
            .has_permission_to_team(api::Z_HasPermissionToTeamArgs {
                a: "u".into(),
                b: "t".into(),
                c: None,
            })
            .await
            .expect("served");
        assert!(!team.a);
        let channel = api
            .has_permission_to_channel(api::Z_HasPermissionToChannelArgs {
                a: "u".into(),
                b: "c".into(),
                c: None,
            })
            .await
            .expect("served");
        assert!(!channel.a);
    }

    /// `GetFileLink` refuses with Go's 501 while public links are off — the default — before
    /// any row is read, so no store is asked.
    #[tokio::test]
    async fn a_file_link_is_refused_while_public_links_are_off() {
        use mm_plugin::rpc::PluginApi as _;
        let api = unreachable_api();
        assert!(!api.app.config().enable_public_link);
        let answer = api
            .get_file_link(api::Z_GetFileLinkArgs { a: "f".into() })
            .await
            .expect("served");
        let err = answer.b.expect("refused");
        assert_eq!(
            (err.id.as_str(), err.status_code, err.r#where.as_str()),
            (
                "plugin_api.get_file_link.disabled.app_error",
                501,
                "GetFileLink"
            )
        );
        assert!(answer.a.is_empty());
    }

    /// The three refusals are served, and past them the mail is attempted: with the database
    /// unreachable the live configuration cannot be read, and the failure comes back as Go's
    /// reused `missing_htmlbody` id rather than as not-implemented.
    #[tokio::test]
    async fn send_mail_refuses_then_sends_and_reports_a_failure_as_go_does() {
        use mm_plugin::rpc::PluginApi as _;
        let api = unreachable_api();
        let refused = api
            .send_mail(api::Z_SendMailArgs {
                a: String::new(),
                b: "s".into(),
                c: "b".into(),
            })
            .await
            .expect("served");
        assert_eq!(
            refused.a.map(|e| e.id),
            Some("plugin_api.send_mail.missing_to".to_owned())
        );
        let sent = api
            .send_mail(api::Z_SendMailArgs {
                a: "a@b".into(),
                b: "s".into(),
                c: "b".into(),
            })
            .await
            .expect("served, not forwarded");
        assert_eq!(
            sent.a.map(|e| e.id),
            Some("plugin_api.send_mail.missing_htmlbody".to_owned())
        );
    }

    /// An `EnsureBot` failure crosses as `encodableError` makes it: a message as an
    /// `ErrorString`, an app error whole, and a wrapped one as its prefix and `Error()` text.
    #[tokio::test]
    async fn an_ensure_bot_error_crosses_as_go_encodes_it() {
        let api = unreachable_api();
        assert_eq!(
            api.ensure_bot_error(EnsureBotError::Message("m".into())),
            PluginError::Message("m".into())
        );
        let app = || AppError::boxed("W", "an.id", None, String::new(), 500);
        match api.ensure_bot_error(EnsureBotError::App(app())) {
            PluginError::App(wire) => {
                assert_eq!((wire.id.as_str(), wire.status_code), ("an.id", 500))
            }
            other => panic!("an app error crosses whole: {other:?}"),
        }
        assert_eq!(
            api.ensure_bot_error(EnsureBotError::Wrapped("failed to patch bot", app())),
            PluginError::Message("failed to patch bot: W: an.id".into())
        );
    }
}
