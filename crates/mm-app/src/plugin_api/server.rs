//! The plugin API's command, plugin, upload-session, team-icon, profile-image, typing, toast,
//! push, channel-restore and cluster methods (app/plugin_api.go), each over the app function a
//! REST route already uses — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! The trait methods in `crate::plugin_api` delegate here one line each; what each answers, and
//! why, is on the function below.
//!
//! # Four methods answer `error`, not `*AppError`
//!
//! `CreateCommand`, `GetCommand`, `UpdateCommand`, `DeleteCommand`, `CreateUploadSession`,
//! `GetUploadSession`, `PublishPluginClusterEvent` and `RegisterCollectionAndTopic` return a Go
//! `error`, so what crosses is `encodableError`'s: an `*AppError` whole, anything else an
//! `ErrorString` with its `Error()` text. The command reads return the **store's** error, not an
//! app error, so a missing command is `resource "Command" not found, id: <id>`.
//!
//! # Go's own shortcuts, kept
//!
//! `CreateCommand` and `UpdateCommand` skip `EnableCommands`; `UpdateCommand` keeps the creator
//! the plugin sent and only takes the team from the stored command when the plugin sent none;
//! `DeleteCommand` succeeds for any id (the store swallows its own error); `SetProfileImage` and
//! `SetTeamIcon` skip `checkImageLimits`, which only the REST upload runs.
//!
//! # Answered without private code, as Go's public build answers
//!
//! `PublishPluginClusterEvent` is `nil` with no cluster, `RegisterCollectionAndTopic` is `nil`
//! always, `GetLDAPUserAttributes` is the 501 of a nil LDAP interface. `LogAuditRec` and
//! `LogAuditRecWithLevel` hand the record to the audit logger, which this server does not keep
//! ([D-1330]); they answer nothing, as Go's do.
//!
//! # What is not implemented, per call
//!
//! Decided before anything is written: `RequestTrialLicense` past its refusals (the licence
//! server's request); an image the pipeline hands to Go (a WebP canvas declaring alpha); a nil
//! upload session or push notification, which Go dereferences.

use std::collections::HashMap;

use gobwire::Interface;
use mm_model::command::Command;
use mm_model::manifest::{
    Manifest, ManifestServer, ManifestWebapp, PluginOption, PluginSetting, PluginSettingsSchema,
    PluginSettingsSection,
};
use mm_model::plugin_status::PluginStatus;
use mm_model::team_member::TeamMemberWithError;
use mm_model::upload_session::{UploadSession, UploadType};
use mm_model::utils::AppError;
use mm_plugin::error::{PluginError, encodable_error};
use mm_plugin::rpc::{NotImplemented, json_to_interface};
use mm_plugin::wire::model as wire;
use mm_plugin::wire::plugin as api;
use mm_store::{CommandStore, StoreError, UserStore};

use super::AppPluginApi;
use crate::channel_member::{ChannelMemberOpts, MemberWrite};
use crate::plugin_commands::{command_from_wire, command_to_wire};
use crate::plugin_hooks::{
    HookContext, channel_member_to_wire, channel_to_wire, push_notification_from_wire,
    team_member_to_wire,
};

/// `app.user.missing_account.const` (app/constants.go:7).
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

/// `store.ErrNotFound.Error()` (store/errors.go:121) for a command.
pub fn command_not_found_text(id: &str) -> String {
    format!("resource \"Command\" not found, id: {id}")
}

/// A `model.Manifest` as gob sends it: every nested pointer as Go holds it, `Props` and each
/// setting's `Default` as the `any` Go's JSON decode of `plugin.json` left.
pub fn manifest_to_wire(manifest: &Manifest) -> wire::Manifest {
    wire::Manifest {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        homepage_url: manifest.homepage_url.clone(),
        support_url: manifest.support_url.clone(),
        release_notes_url: manifest.release_notes_url.clone(),
        icon_path: manifest.icon_path.clone(),
        version: manifest.version.clone(),
        min_server_version: manifest.min_server_version.clone(),
        server: manifest
            .server
            .as_ref()
            .map(|s| Box::new(server_to_wire(s))),
        webapp: manifest
            .webapp
            .as_ref()
            .map(|w| Box::new(webapp_to_wire(w))),
        settings_schema: manifest
            .settings_schema
            .as_ref()
            .map(|s| Box::new(schema_to_wire(s))),
        props: manifest
            .props
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), json_to_interface(v)))
            .collect(),
    }
}

fn server_to_wire(server: &ManifestServer) -> wire::ManifestServer {
    wire::ManifestServer {
        executables: server
            .executables
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        executable: server.executable.clone(),
    }
}

fn webapp_to_wire(webapp: &ManifestWebapp) -> wire::ManifestWebapp {
    wire::ManifestWebapp {
        bundle_path: webapp.bundle_path.clone(),
        bundle_hash: webapp.bundle_hash.clone(),
    }
}

fn schema_to_wire(schema: &PluginSettingsSchema) -> wire::PluginSettingsSchema {
    wire::PluginSettingsSchema {
        header: schema.header.clone(),
        footer: schema.footer.clone(),
        settings: settings_to_wire(schema.settings.as_deref()),
        sections: schema
            .sections
            .iter()
            .flatten()
            .map(section_to_wire)
            .collect(),
    }
}

fn section_to_wire(section: &PluginSettingsSection) -> wire::PluginSettingsSection {
    wire::PluginSettingsSection {
        key: section.key.clone(),
        title: section.title.clone(),
        subtitle: section.subtitle.clone(),
        settings: settings_to_wire(section.settings.as_deref()),
        header: section.header.clone(),
        footer: section.footer.clone(),
        custom: section.custom,
        fallback: section.fallback,
    }
}

fn settings_to_wire(settings: Option<&[PluginSetting]>) -> Vec<wire::PluginSetting> {
    settings
        .into_iter()
        .flatten()
        .map(|setting| wire::PluginSetting {
            key: setting.key.clone(),
            display_name: setting.display_name.clone(),
            r#type: setting.type_.clone(),
            help_text: setting.help_text.clone(),
            regenerate_help_text: setting.regenerate_help_text.clone(),
            placeholder: setting.placeholder.clone(),
            default: json_to_interface(&setting.default),
            options: setting
                .options
                .iter()
                .flatten()
                .map(|o: &PluginOption| wire::PluginOption {
                    display_name: o.display_name.clone(),
                    value: o.value.clone(),
                })
                .collect(),
            hosting: setting.hosting.clone(),
            secret: setting.secret,
        })
        .collect()
}

/// A `model.PluginStatus` as gob sends it.
pub fn plugin_status_to_wire(status: &PluginStatus) -> wire::PluginStatus {
    wire::PluginStatus {
        plugin_id: status.plugin_id.clone(),
        cluster_id: status.cluster_id.clone(),
        plugin_path: status.plugin_path.clone(),
        state: status.state,
        error: status.error.clone(),
        name: status.name.clone(),
        description: status.description.clone(),
        version: status.version.clone(),
    }
}

/// A `model.UploadSession` as gob sends it, and back.
pub fn upload_session_to_wire(us: &UploadSession) -> wire::UploadSession {
    wire::UploadSession {
        id: us.id.clone(),
        r#type: us.type_.as_str().to_owned(),
        create_at: us.create_at,
        user_id: us.user_id.clone(),
        channel_id: us.channel_id.clone(),
        filename: us.filename.clone(),
        path: us.path.clone(),
        file_size: us.file_size,
        file_offset: us.file_offset,
        remote_id: us.remote_id.clone(),
        req_file_id: us.req_file_id.clone(),
    }
}

pub fn upload_session_from_wire(wire: &wire::UploadSession) -> UploadSession {
    UploadSession {
        id: wire.id.clone(),
        type_: UploadType(wire.r#type.clone()),
        create_at: wire.create_at,
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        filename: wire.filename.clone(),
        path: wire.path.clone(),
        file_size: wire.file_size,
        file_offset: wire.file_offset,
        remote_id: wire.remote_id.clone(),
        req_file_id: wire.req_file_id.clone(),
    }
}

/// `PluginAPI.UpdateCommand`'s merge (app/plugin_api.go:1465): the plugin's command, with the
/// stored id, token, create and delete times, a fresh update time, this plugin's id, and the
/// stored team only when the plugin sent none. The creator is the **plugin's**, unlike
/// `App.UpdateCommand`, which keeps the stored one.
pub fn plugin_command_update(old: &Command, mut updated: Command, plugin_id: &str) -> Command {
    updated.trigger = updated.trigger.to_lowercase();
    updated.id.clone_from(&old.id);
    updated.token.clone_from(&old.token);
    updated.create_at = old.create_at;
    updated.update_at = mm_model::utils::get_millis();
    updated.delete_at = old.delete_at;
    updated.plugin_id = plugin_id.to_owned();
    if updated.team_id.is_empty() {
        updated.team_id.clone_from(&old.team_id);
    }
    updated
}

/// `Channels.RequestTrialLicense`'s refusals (app/license.go:63), before the licence server is
/// asked: `ExperimentalSettings.RestrictSystemAdmin` (the plugin API's own check, first), the
/// terms, a zero user count. The requester's lookup follows in the caller.
pub fn trial_license_refusal(
    restrict_system_admin: bool,
    users: i64,
    terms_accepted: bool,
) -> Option<Box<AppError>> {
    if restrict_system_admin {
        return Some(AppError::boxed(
            "RequestTrialLicense",
            "api.restricted_system_admin",
            None,
            "",
            403,
        ));
    }
    if !terms_accepted {
        return Some(AppError::boxed(
            "RequestTrialLicense",
            "api.license.request-trial.bad-request.terms-not-accepted",
            None,
            "",
            400,
        ));
    }
    if users == 0 {
        return Some(AppError::boxed(
            "RequestTrialLicense",
            "api.license.request-trial.bad-request",
            None,
            "",
            400,
        ));
    }
    None
}

/// A `TeamMemberWithError` as gob sends it.
fn team_member_with_error_to_wire(
    api: &AppPluginApi,
    entry: TeamMemberWithError,
) -> wire::TeamMemberWithError {
    wire::TeamMemberWithError {
        user_id: entry.user_id,
        member: entry
            .member
            .as_ref()
            .map(|m| Box::new(team_member_to_wire(m))),
        error: entry.error.and_then(|e| api.wire(e)),
    }
}

impl AppPluginApi {
    /// An `*AppError` in an `error` return: translated, then crossing whole.
    fn app_error_as_error(&self, err: Box<AppError>) -> Option<Interface> {
        self.wire(err)
            .and_then(|w| encodable_error(Some(&PluginError::App(w))))
    }

    /// A plain Go `error` in an `error` return: its text in an `ErrorString`.
    fn message_as_error(text: String) -> Option<Interface> {
        encodable_error(Some(&PluginError::Message(text)))
    }

    /// A store error as Go's command store would have returned it.
    fn command_store_error(&self, err: StoreError, id: &str) -> Option<Interface> {
        match err {
            StoreError::Invalid { app_error, .. } => self.app_error_as_error(app_error),
            StoreError::NotFound { .. } => Self::message_as_error(command_not_found_text(id)),
            other => Self::message_as_error(other.to_string()),
        }
    }

    // -- channels -------------------------------------------------------------------------

    /// Port of `PluginAPI.AddUserToChannel` (app/plugin_api.go:702): `AddChannelMember` with
    /// the third argument as the requestor, so the system post says who added the user.
    pub(super) async fn server_add_user_to_channel(
        &self,
        args: api::Z_AddUserToChannelArgs,
    ) -> Result<api::Z_AddUserToChannelReturns, NotImplemented> {
        let channel = match self.resolve_channel(&args.a).await {
            Ok(channel) => channel,
            Err(err) => {
                return Ok(api::Z_AddUserToChannelReturns {
                    a: None,
                    b: self.wire(err),
                });
            }
        };
        let opts = ChannelMemberOpts {
            user_requestor_id: args.c.clone(),
            ..ChannelMemberOpts::default()
        };
        match self
            .app
            .add_channel_member(&args.b, &channel, &opts, &HookContext::default())
            .await
        {
            Ok(MemberWrite::Done(member)) => Ok(api::Z_AddUserToChannelReturns {
                a: Some(Box::new(channel_member_to_wire(&member))),
                b: None,
            }),
            Ok(MemberWrite::Forward(why)) => Err(self.not_implemented("AddUserToChannel", why)),
            Err(err) => Ok(api::Z_AddUserToChannelReturns {
                a: None,
                b: self.wire(err),
            }),
        }
    }

    /// Port of `PluginAPI.GetChannelOfType` (app/plugin_api.go:510): the one read that reaches a
    /// channel by id whatever its type, and refuses one of another type as missing.
    pub(super) async fn server_get_channel_of_type(
        &self,
        args: api::Z_GetChannelOfTypeArgs,
    ) -> Result<api::Z_GetChannelOfTypeReturns, NotImplemented> {
        let (a, b) = self.reply(
            self.app.get_channel_of_type(&args.a, &args.b).await,
            |channel| channel_to_wire(&channel),
        );
        Ok(api::Z_GetChannelOfTypeReturns { a, b })
    }

    /// Port of `PluginAPI.RestoreChannel` (app/plugin_api.go:486): `RestoreChannel` with no
    /// user, so no unarchive post is written — only the `channel_restored` event.
    pub(super) async fn server_restore_channel(
        &self,
        args: api::Z_RestoreChannelArgs,
    ) -> Result<api::Z_RestoreChannelReturns, NotImplemented> {
        let result = match self.resolve_channel(&args.a).await {
            Ok(mut channel) => {
                self.app
                    .restore_channel(&HookContext::default(), &mut channel, "")
                    .await
            }
            Err(err) => Err(err),
        };
        Ok(api::Z_RestoreChannelReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- teams ----------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateTeamMembersGracefully` (app/plugin_api.go:236): `AddTeamMembers`
    /// with `graceful`, so each user's failure is its own entry and the call succeeds.
    pub(super) async fn server_create_team_members_gracefully(
        &self,
        args: api::Z_CreateTeamMembersGracefullyArgs,
    ) -> Result<api::Z_CreateTeamMembersGracefullyReturns, NotImplemented> {
        let result = self
            .app
            .add_team_members(&args.a, &args.b, &args.c, true, &HookContext::default())
            .await;
        Ok(match result {
            Ok(entries) => api::Z_CreateTeamMembersGracefullyReturns {
                a: entries
                    .into_iter()
                    .map(|e| team_member_with_error_to_wire(self, e))
                    .collect(),
                b: None,
            },
            Err(err) => api::Z_CreateTeamMembersGracefullyReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        })
    }

    /// Port of `PluginAPI.GetTeamIcon` (app/plugin_api.go:1095): `GetTeam`, then the stored
    /// icon, whose absence is `GetTeamIcon`'s 404.
    pub(super) async fn server_get_team_icon(
        &self,
        args: api::Z_GetTeamIconArgs,
    ) -> Result<api::Z_GetTeamIconReturns, NotImplemented> {
        if let Err(err) = self.app.get_team(&args.a).await {
            return Ok(api::Z_GetTeamIconReturns {
                a: Vec::new(),
                b: self.wire(err),
            });
        }
        let result = self.served("GetTeamIcon", self.app.get_team_icon(&args.a).await)?;
        Ok(match result {
            Ok(data) => api::Z_GetTeamIconReturns { a: data, b: None },
            Err(err) => api::Z_GetTeamIconReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        })
    }

    /// Port of `PluginAPI.SetTeamIcon` (app/plugin_api.go:1108): `GetTeam`, then
    /// [`crate::App::set_team_icon_from_file`] — no `checkImageLimits`.
    pub(super) async fn server_set_team_icon(
        &self,
        args: api::Z_SetTeamIconArgs,
    ) -> Result<api::Z_SetTeamIconReturns, NotImplemented> {
        let team = match self.app.get_team(&args.a).await {
            Ok(team) => team,
            Err(err) => return Ok(api::Z_SetTeamIconReturns { a: self.wire(err) }),
        };
        let result = self.served(
            "SetTeamIcon",
            self.app.set_team_icon_from_file(&team, &args.b).await,
        )?;
        Ok(api::Z_SetTeamIconReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.RemoveTeamIcon` (app/plugin_api.go:1121): `GetTeam`'s own error first,
    /// then `RemoveTeamIcon`, which looks the team up again and wraps a miss in its 400.
    pub(super) async fn server_remove_team_icon(
        &self,
        args: api::Z_RemoveTeamIconArgs,
    ) -> Result<api::Z_RemoveTeamIconReturns, NotImplemented> {
        let result = match self.app.get_team(&args.a).await {
            Ok(_) => self.app.remove_team_icon(&args.a).await,
            Err(err) => Err(err),
        };
        Ok(api::Z_RemoveTeamIconReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- users ----------------------------------------------------------------------------

    /// Port of `PluginAPI.SetProfileImage` (app/plugin_api.go:1026): `GetUser`, then
    /// [`crate::App::set_profile_image_from_file`] — no `checkImageLimits`.
    pub(super) async fn server_set_profile_image(
        &self,
        args: api::Z_SetProfileImageArgs,
    ) -> Result<api::Z_SetProfileImageReturns, NotImplemented> {
        if let Err(err) = self.app.get_user(&args.a).await {
            return Ok(api::Z_SetProfileImageReturns { a: self.wire(err) });
        }
        let result = self.served(
            "SetProfileImage",
            self.app.set_profile_image_from_file(&args.a, &args.b).await,
        )?;
        Ok(api::Z_SetProfileImageReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.PublishUserTyping` (app/plugin_api.go:1332): the `typing` event,
    /// without a check of the user or the channel.
    pub(super) async fn server_publish_user_typing(
        &self,
        args: api::Z_PublishUserTypingArgs,
    ) -> Result<api::Z_PublishUserTypingReturns, NotImplemented> {
        let result = self
            .app
            .publish_user_typing(&args.a, &args.b, &args.c)
            .await;
        Ok(api::Z_PublishUserTypingReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.SendToastMessage` (app/plugin_api.go:1246); see
    /// [`crate::App::send_toast_message`].
    pub(super) async fn server_send_toast_message(
        &self,
        args: api::Z_SendToastMessageArgs,
    ) -> Result<api::Z_SendToastMessageReturns, NotImplemented> {
        let result = self
            .app
            .send_toast_message(&args.a, &args.b, &args.c, &args.d.position)
            .await;
        Ok(api::Z_SendToastMessageReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.SendPushNotification` (app/plugin_api.go:1603):
    /// `sendPushNotificationToAllSessions` with no session skipped. A nil notification is a
    /// dereference in Go, and not implemented here.
    pub(super) async fn server_send_push_notification(
        &self,
        args: api::Z_SendPushNotificationArgs,
    ) -> Result<api::Z_SendPushNotificationReturns, NotImplemented> {
        let Some(notification) = args.a.as_deref() else {
            return Err(self.not_implemented("SendPushNotification", "a nil notification"));
        };
        let result = self
            .app
            .send_push_notification_to_all_sessions(
                push_notification_from_wire(notification),
                &args.b,
                "",
            )
            .await;
        Ok(api::Z_SendPushNotificationReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.GetLDAPUserAttributes` (app/plugin_api.go:445): the LDAP interface is
    /// private code, nil on every public build, so this is always the first branch's 501.
    pub(super) async fn server_get_ldap_user_attributes(
        &self,
        _args: api::Z_GetLDAPUserAttributesArgs,
    ) -> Result<api::Z_GetLDAPUserAttributesReturns, NotImplemented> {
        Ok(api::Z_GetLDAPUserAttributesReturns {
            a: HashMap::new(),
            b: self.wire(AppError::boxed(
                "GetLdapUserAttributes",
                "ent.ldap.disabled.app_error",
                None,
                "",
                501,
            )),
        })
    }

    /// Port of `PluginAPI.RequestTrialLicense` (app/plugin_api.go:1552) up to the licence
    /// server: [`trial_license_refusal`], then the requester's lookup. Past both, Go asks the
    /// licence server, which is not implemented here.
    pub(super) async fn server_request_trial_license(
        &self,
        args: api::Z_RequestTrialLicenseArgs,
    ) -> Result<api::Z_RequestTrialLicenseReturns, NotImplemented> {
        let restrict = self.app.config().restrict_system_admin;
        if let Some(err) = trial_license_refusal(restrict, args.b, args.c) {
            return Ok(api::Z_RequestTrialLicenseReturns { a: self.wire(err) });
        }
        if let Err(err) = self.app.store().user().get(&args.a).await {
            let err = if err.is_not_found() {
                AppError::boxed("RequestTrialLicense", MISSING_ACCOUNT_ERROR, None, "", 404)
            } else {
                AppError::boxed(
                    "RequestTrialLicense",
                    "app.user.get_by_username.app_error",
                    None,
                    "",
                    500,
                )
            };
            return Ok(api::Z_RequestTrialLicenseReturns { a: self.wire(err) });
        }
        Err(self.not_implemented("RequestTrialLicense", "the licence server's trial request"))
    }

    // -- plugins --------------------------------------------------------------------------

    /// Port of `PluginAPI.GetPlugins` (app/plugin_api.go:1158): the active plugins' manifests,
    /// then the inactive ones', each in the environment's order.
    pub(super) async fn server_get_plugins(
        &self,
        _args: api::Z_GetPluginsArgs,
    ) -> Result<api::Z_GetPluginsReturns, NotImplemented> {
        Ok(match self.app.get_plugins() {
            Ok(plugins) => api::Z_GetPluginsReturns {
                a: plugins
                    .active
                    .iter()
                    .flatten()
                    .chain(plugins.inactive.iter().flatten())
                    .map(|info| manifest_to_wire(&info.manifest))
                    .collect(),
                b: None,
            },
            Err(err) => api::Z_GetPluginsReturns {
                a: Vec::new(),
                b: self.wire(err),
            },
        })
    }

    /// Port of `PluginAPI.GetPluginStatus` (app/plugin_api.go:1185).
    pub(super) async fn server_get_plugin_status(
        &self,
        args: api::Z_GetPluginStatusArgs,
    ) -> Result<api::Z_GetPluginStatusReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_plugin_status(&args.a), |s| {
            plugin_status_to_wire(&s)
        });
        Ok(api::Z_GetPluginStatusReturns { a, b })
    }

    /// Port of `PluginAPI.EnablePlugin` (app/plugin_api.go:1173).
    pub(super) async fn server_enable_plugin(
        &self,
        args: api::Z_EnablePluginArgs,
    ) -> Result<api::Z_EnablePluginReturns, NotImplemented> {
        let result = self.app.enable_plugin(&args.a).await;
        Ok(api::Z_EnablePluginReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.DisablePlugin` (app/plugin_api.go:1177).
    pub(super) async fn server_disable_plugin(
        &self,
        args: api::Z_DisablePluginArgs,
    ) -> Result<api::Z_DisablePluginReturns, NotImplemented> {
        let result = self.app.disable_plugin(&args.a).await;
        Ok(api::Z_DisablePluginReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.RemovePlugin` (app/plugin_api.go:1181).
    pub(super) async fn server_remove_plugin(
        &self,
        args: api::Z_RemovePluginArgs,
    ) -> Result<api::Z_RemovePluginReturns, NotImplemented> {
        let result = self.app.remove_plugin(&args.a).await;
        Ok(api::Z_RemovePluginReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- commands -------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateCommand` (app/plugin_api.go:1387): no creator, this plugin as
    /// the owner, and the unexported `createCommand` — no `EnableCommands` gate.
    pub(super) async fn server_create_command(
        &self,
        args: api::Z_CreateCommandArgs,
    ) -> Result<api::Z_CreateCommandReturns, NotImplemented> {
        let Some(wire_command) = args.a.as_deref() else {
            return Err(self.not_implemented("CreateCommand", "a nil command"));
        };
        let mut command = command_from_wire(wire_command);
        command.creator_id.clear();
        command.plugin_id.clone_from(&self.id);
        Ok(match self.app.create_command_ungated(command).await {
            Ok(saved) => api::Z_CreateCommandReturns {
                a: Some(Box::new(command_to_wire(&saved))),
                b: None,
            },
            Err(err) => api::Z_CreateCommandReturns {
                a: None,
                b: self.app_error_as_error(err),
            },
        })
    }

    /// Port of `PluginAPI.GetCommand` (app/plugin_api.go:1461): the store's `Get`, whose miss is
    /// the store's own `ErrNotFound`.
    pub(super) async fn server_get_command(
        &self,
        args: api::Z_GetCommandArgs,
    ) -> Result<api::Z_GetCommandReturns, NotImplemented> {
        Ok(match self.app.store().command().get(&args.a).await {
            Ok(command) => api::Z_GetCommandReturns {
                a: Some(Box::new(command_to_wire(&command))),
                b: None,
            },
            Err(err) => api::Z_GetCommandReturns {
                a: None,
                b: self.command_store_error(err, &args.a),
            },
        })
    }

    /// Port of `PluginAPI.UpdateCommand` (app/plugin_api.go:1465); see
    /// [`plugin_command_update`]. Then the trigger's uniqueness, then the store's `Update`.
    pub(super) async fn server_update_command(
        &self,
        args: api::Z_UpdateCommandArgs,
    ) -> Result<api::Z_UpdateCommandReturns, NotImplemented> {
        let old = match self.app.store().command().get(&args.a).await {
            Ok(command) => command,
            Err(err) => {
                return Ok(api::Z_UpdateCommandReturns {
                    a: None,
                    b: self.command_store_error(err, &args.a),
                });
            }
        };
        let Some(wire_command) = args.b.as_deref() else {
            return Err(self.not_implemented("UpdateCommand", "a nil command"));
        };
        let mut updated = plugin_command_update(&old, command_from_wire(wire_command), &self.id);
        if let Err(err) = self
            .app
            .validate_command_trigger_uniqueness(&updated.team_id, &updated.trigger, &updated.id)
            .await
        {
            return Ok(api::Z_UpdateCommandReturns {
                a: None,
                b: self.app_error_as_error(err),
            });
        }
        Ok(
            match self.app.store().command().update(&mut updated).await {
                Ok(()) => api::Z_UpdateCommandReturns {
                    a: Some(Box::new(command_to_wire(&updated))),
                    b: None,
                },
                Err(err) => api::Z_UpdateCommandReturns {
                    a: None,
                    b: self.command_store_error(err, &args.a),
                },
            },
        )
    }

    /// Port of `PluginAPI.DeleteCommand` (app/plugin_api.go:1489): the store's soft delete,
    /// which swallows its own error, so any id succeeds.
    pub(super) async fn server_delete_command(
        &self,
        args: api::Z_DeleteCommandArgs,
    ) -> Result<api::Z_DeleteCommandReturns, NotImplemented> {
        if let Err(err) = self
            .app
            .store()
            .command()
            .delete(&args.a, mm_model::utils::get_millis())
            .await
        {
            tracing::warn!(error = %err, "the plugin's command delete failed, as Go ignores");
        }
        Ok(api::Z_DeleteCommandReturns { a: None })
    }

    // -- uploads --------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateUploadSession` (app/plugin_api.go:1577).
    pub(super) async fn server_create_upload_session(
        &self,
        args: api::Z_CreateUploadSessionArgs,
    ) -> Result<api::Z_CreateUploadSessionReturns, NotImplemented> {
        let Some(us) = args.a.as_deref() else {
            return Err(self.not_implemented("CreateUploadSession", "a nil upload session"));
        };
        Ok(
            match self
                .app
                .create_upload_session(upload_session_from_wire(us))
                .await
            {
                Ok(saved) => api::Z_CreateUploadSessionReturns {
                    a: Some(Box::new(upload_session_to_wire(&saved))),
                    b: None,
                },
                Err(err) => api::Z_CreateUploadSessionReturns {
                    a: None,
                    b: self.app_error_as_error(err),
                },
            },
        )
    }

    /// Port of `PluginAPI.GetUploadSession` (app/plugin_api.go:1593).
    pub(super) async fn server_get_upload_session(
        &self,
        args: api::Z_GetUploadSessionArgs,
    ) -> Result<api::Z_GetUploadSessionReturns, NotImplemented> {
        Ok(match self.app.get_upload_session(&args.a).await {
            Ok(us) => api::Z_GetUploadSessionReturns {
                a: Some(Box::new(upload_session_to_wire(&us))),
                b: None,
            },
            Err(err) => api::Z_GetUploadSessionReturns {
                a: None,
                b: self.app_error_as_error(err),
            },
        })
    }

    // -- the cluster and the audit log ----------------------------------------------------

    /// Port of `PluginAPI.PublishPluginClusterEvent` (app/plugin_api.go:1521): no cluster, so
    /// `nil` before anything else.
    pub(super) async fn server_publish_plugin_cluster_event(
        &self,
        _args: api::Z_PublishPluginClusterEventArgs,
    ) -> Result<api::Z_PublishPluginClusterEventReturns, NotImplemented> {
        Ok(api::Z_PublishPluginClusterEventReturns { a: None })
    }

    /// Port of `PluginAPI.RegisterCollectionAndTopic` (app/plugin_api.go:1573): `nil`.
    pub(super) async fn server_register_collection_and_topic(
        &self,
        _args: api::Z_RegisterCollectionAndTopicArgs,
    ) -> Result<api::Z_RegisterCollectionAndTopicReturns, NotImplemented> {
        Ok(api::Z_RegisterCollectionAndTopicReturns { a: None })
    }

    /// Port of `PluginAPI.LogAuditRecWithLevel` (app/plugin_api.go:213): Go adds `plugin_id`
    /// to the record's parameters and hands it to the audit logger; this server keeps no audit
    /// log ([D-1330]), so the record is logged here at debug and goes no further.
    pub(super) fn server_log_audit_rec(&self, record: Option<&wire::AuditRecord>, level: &str) {
        let Some(record) = record else {
            return;
        };
        tracing::debug!(
            plugin_id = %self.id,
            event = %record.event_name,
            status = %record.status,
            level,
            "a plugin's audit record; this server keeps no audit log"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_command_is_the_stores_text() {
        assert_eq!(
            command_not_found_text("abc"),
            "resource \"Command\" not found, id: abc"
        );
    }

    #[test]
    fn the_update_keeps_the_plugins_creator_and_the_stored_team_only_when_none_was_sent() {
        let old = Command {
            id: "old".into(),
            token: "tok".into(),
            create_at: 1,
            delete_at: 2,
            creator_id: "stored-creator".into(),
            team_id: "stored-team".into(),
            plugin_id: "other".into(),
            ..Command::default()
        };
        let sent = Command {
            id: "ignored".into(),
            trigger: "UP".into(),
            creator_id: "plugin-creator".into(),
            ..Command::default()
        };
        let merged = plugin_command_update(&old, sent, "me");
        assert_eq!(
            (
                merged.id.as_str(),
                merged.token.as_str(),
                merged.create_at,
                merged.delete_at
            ),
            ("old", "tok", 1, 2)
        );
        assert_eq!(merged.trigger, "up");
        assert_eq!(merged.creator_id, "plugin-creator");
        assert_eq!(merged.team_id, "stored-team");
        assert_eq!(merged.plugin_id, "me");
        let with_team = plugin_command_update(
            &old,
            Command {
                team_id: "sent".into(),
                ..Command::default()
            },
            "me",
        );
        assert_eq!(with_team.team_id, "sent");
    }

    #[test]
    fn the_trial_refusals_are_in_go_s_order() {
        let id = |e: Option<Box<AppError>>| e.map(|e| (e.id, e.status_code));
        assert_eq!(
            id(trial_license_refusal(true, 0, false)),
            Some(("api.restricted_system_admin".to_owned(), 403))
        );
        assert_eq!(
            id(trial_license_refusal(false, 0, false)),
            Some((
                "api.license.request-trial.bad-request.terms-not-accepted".to_owned(),
                400
            ))
        );
        assert_eq!(
            id(trial_license_refusal(false, 0, true)),
            Some(("api.license.request-trial.bad-request".to_owned(), 400))
        );
        assert_eq!(id(trial_license_refusal(false, 10, true)), None);
    }
}
