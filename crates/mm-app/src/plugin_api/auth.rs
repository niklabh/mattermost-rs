//! The plugin API's session, access-token, auth-data, OAuth-app, role and group methods
//! (app/plugin_api.go), each a thin wrapper over the app function a REST route already uses, or
//! over `crate::group_lookup` where no served route reached one — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! The trait methods in `crate::plugin_api` delegate here one line each; what each answers, and
//! why, is on the function below.
//!
//! # Most group methods are behind `checkLDAPLicense`
//!
//! Every group method but the five reads `GetGroup`, `GetGroupByName`, `GetGroupMemberUsers`,
//! `GetGroupsBySource` and `GetGroupsForUser` opens with `PluginAPI.checkLDAPLicense`
//! (app/plugin_api.go:42): no licence, or one without `Features.LDAPGroups`, is
//! `app.group.license_error` at **403** whose `Where` is the method's own name. Gob carries
//! `Where`, so the name is on the wire. The five ungated reads answer on an unlicensed server.
//!
//! # `Where` is Go's, which is not always the app function's
//!
//! `RevokeSession` is `RevokeSessionById`, which re-wraps `GetSessionById`'s miss under its own
//! name; `ExtendSessionExpiry` builds its two errors itself with the lower-case
//! `extendSessionExpiry`. A REST handler overwrites `Where`, so neither difference was visible
//! until a plugin read it.
//!
//! # What is not implemented, per call
//!
//! Decided before anything is written: `RevokeSession` of an **OAuth** session, whose Go path
//! (`RevokeAccessToken`) deletes the `OAuthAccessData` row this server has no store for — see
//! `App::revoke_session` and [D-283].
//!
//! # One method is compared only where it refuses
//!
//! `DeleteGroupConstrainedMemberships` sweeps every group-constrained team and channel on the
//! installation, which the parity suites share, so the auth tranche calls it only unlicensed.
//! Behind the gate it is `App::delete_group_constrained_memberships`, the same two sweeps the
//! REST unlink dispatches scoped to one syncable ([D-1070]).

use mm_model::group::{
    CreateDefaultMembershipParams, Group, GroupSearchOpts, GroupSource, PageOpts,
};
use mm_model::group_member::GroupMember;
use mm_model::group_syncable::{GroupSyncable, GroupSyncableType};
use mm_model::oauth::OAuthApp;
use mm_model::session::Session;
use mm_model::user::{UserAuth, ViewUsersRestrictions};
use mm_model::user_access_token::UserAccessToken;
use mm_model::utils::AppError;
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model as wire;
use mm_plugin::wire::plugin as api;
use mm_store::SessionStore;

use super::AppPluginApi;
use crate::plugin_api_wire::session_to_wire;
use crate::plugin_hooks::{HookContext, user_to_wire};

/// `checkLDAPLicense`'s refusal as each gated method wraps it (app/plugin_api.go:806 and on).
pub fn auth_license_error(method: &str) -> Box<AppError> {
    AppError::boxed(method, "app.group.license_error", None, String::new(), 403)
}

/// `PluginAPI.checkLDAPLicense` (app/plugin_api.go:42): a licence, with `Features.LDAPGroups`.
pub fn auth_ldap_groups_licensed(license: Option<&mm_model::license::License>) -> bool {
    license
        .and_then(|l| l.features.as_ref())
        .and_then(|f| f.ldap_groups)
        .unwrap_or(false)
}

/// A session as the plugin sent it. `TeamMembers` is `db:"-"` and the store's `Save` replaces
/// it, so it is not read.
pub fn auth_session_from_wire(wire: &wire::Session) -> Session {
    Session {
        id: wire.id.clone(),
        token: wire.token.clone(),
        create_at: wire.create_at,
        expires_at: wire.expires_at,
        last_activity_at: wire.last_activity_at,
        user_id: wire.user_id.clone(),
        device_id: wire.device_id.clone(),
        voip_device_id: wire.vo_ip_device_id.clone(),
        roles: wire.roles.clone(),
        is_oauth: wire.is_o_auth,
        expired_notify: wire.expired_notify,
        // Gob sends an empty map as nothing, so it arrives as Go's nil either way.
        props: (!wire.props.is_empty()).then(|| {
            wire.props
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }),
        team_members: None,
        local: wire.local,
    }
}

/// A token as gob sends it (`model.UserAccessToken`, user_access_token.go:15).
pub fn auth_token_to_wire(token: &UserAccessToken) -> wire::UserAccessToken {
    wire::UserAccessToken {
        id: token.id.clone(),
        token: token.token.clone(),
        user_id: token.user_id.clone(),
        description: token.description.clone(),
        is_active: token.is_active,
        expires_at: token.expires_at,
        last_notified_at: token.last_notified_at,
    }
}

pub fn auth_token_from_wire(wire: &wire::UserAccessToken) -> UserAccessToken {
    UserAccessToken {
        id: wire.id.clone(),
        token: wire.token.clone(),
        user_id: wire.user_id.clone(),
        description: wire.description.clone(),
        is_active: wire.is_active,
        expires_at: wire.expires_at,
        last_notified_at: wire.last_notified_at,
    }
}

/// An OAuth app as gob sends it: the secret included, as Go's `GetApp` reads it.
pub fn auth_oauth_app_to_wire(app: &OAuthApp) -> wire::OAuthApp {
    wire::OAuthApp {
        id: app.id.clone(),
        creator_id: app.creator_id.clone(),
        create_at: app.create_at,
        update_at: app.update_at,
        client_secret: app.client_secret.clone(),
        name: app.name.clone(),
        description: app.description.clone(),
        icon_url: app.icon_url.clone(),
        callback_urls: app.callback_urls.clone().unwrap_or_default(),
        homepage: app.homepage.clone(),
        is_trusted: app.is_trusted,
        mattermost_app_id: app.mattermost_app_id.clone(),
        is_dynamically_registered: app.is_dynamically_registered,
    }
}

/// The reverse; an empty callback list crosses gob as Go's nil.
pub fn auth_oauth_app_from_wire(wire: &wire::OAuthApp) -> OAuthApp {
    OAuthApp {
        id: wire.id.clone(),
        creator_id: wire.creator_id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        client_secret: wire.client_secret.clone(),
        name: wire.name.clone(),
        description: wire.description.clone(),
        icon_url: wire.icon_url.clone(),
        callback_urls: (!wire.callback_urls.is_empty()).then(|| wire.callback_urls.clone()),
        homepage: wire.homepage.clone(),
        is_trusted: wire.is_trusted,
        mattermost_app_id: wire.mattermost_app_id.clone(),
        is_dynamically_registered: wire.is_dynamically_registered,
    }
}

/// A group as gob sends it (`model.Group`, group.go:37). A nil `MemberIDs` and an empty one are
/// one value on the wire.
pub fn auth_group_to_wire(group: &Group) -> wire::Group {
    wire::Group {
        id: group.id.clone(),
        name: group.name.clone(),
        display_name: group.display_name.clone(),
        description: group.description.clone(),
        source: group.source.as_str().to_owned(),
        remote_id: group.remote_id.clone(),
        create_at: group.create_at,
        update_at: group.update_at,
        delete_at: group.delete_at,
        has_syncables: group.has_syncables,
        member_count: group.member_count,
        allow_reference: group.allow_reference,
        channel_member_count: group.channel_member_count,
        channel_member_timezones_count: group.channel_member_timezones_count,
        member_i_ds: group.member_ids.clone().unwrap_or_default(),
    }
}

pub fn auth_group_from_wire(wire: &wire::Group) -> Group {
    Group {
        id: wire.id.clone(),
        name: wire.name.clone(),
        display_name: wire.display_name.clone(),
        description: wire.description.clone(),
        source: GroupSource(wire.source.clone()),
        remote_id: wire.remote_id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        has_syncables: wire.has_syncables,
        member_count: wire.member_count,
        allow_reference: wire.allow_reference,
        channel_member_count: wire.channel_member_count,
        channel_member_timezones_count: wire.channel_member_timezones_count,
        member_ids: (!wire.member_i_ds.is_empty()).then(|| wire.member_i_ds.clone()),
    }
}

pub fn auth_group_member_to_wire(member: &GroupMember) -> wire::GroupMember {
    wire::GroupMember {
        group_id: member.group_id.clone(),
        user_id: member.user_id.clone(),
        create_at: member.create_at,
        delete_at: member.delete_at,
    }
}

pub fn auth_group_syncable_to_wire(gs: &GroupSyncable) -> wire::GroupSyncable {
    wire::GroupSyncable {
        group_id: gs.group_id.clone(),
        syncable_id: gs.syncable_id.clone(),
        auto_add: gs.auto_add,
        scheme_admin: gs.scheme_admin,
        create_at: gs.create_at,
        delete_at: gs.delete_at,
        update_at: gs.update_at,
        r#type: gs.type_.as_str().to_owned(),
        channel_display_name: gs.channel_display_name.clone(),
        team_display_name: gs.team_display_name.clone(),
        team_type: gs.team_type.clone(),
        channel_type: gs.channel_type.clone(),
        team_id: gs.team_id.clone(),
    }
}

pub fn auth_group_syncable_from_wire(wire: &wire::GroupSyncable) -> GroupSyncable {
    GroupSyncable {
        group_id: wire.group_id.clone(),
        syncable_id: wire.syncable_id.clone(),
        auto_add: wire.auto_add,
        scheme_admin: wire.scheme_admin,
        create_at: wire.create_at,
        delete_at: wire.delete_at,
        update_at: wire.update_at,
        type_: GroupSyncableType(wire.r#type.clone()),
        channel_display_name: wire.channel_display_name.clone(),
        team_display_name: wire.team_display_name.clone(),
        team_type: wire.team_type.clone(),
        channel_type: wire.channel_type.clone(),
        team_id: wire.team_id.clone(),
    }
}

pub fn auth_group_search_opts_from_wire(wire: &wire::GroupSearchOpts) -> GroupSearchOpts {
    GroupSearchOpts {
        q: wire.q.clone(),
        not_associated_to_team: wire.not_associated_to_team.clone(),
        not_associated_to_channel: wire.not_associated_to_channel.clone(),
        include_member_count: wire.include_member_count,
        filter_allow_reference: wire.filter_allow_reference,
        page_opts: wire.page_opts.as_deref().map(|p| PageOpts {
            page: p.page,
            per_page: p.per_page,
        }),
        since: wire.since,
        source: GroupSource(wire.source.clone()),
        filter_parent_team_permitted: wire.filter_parent_team_permitted,
        filter_has_member: wire.filter_has_member.clone(),
        include_channel_member_count: wire.include_channel_member_count.clone(),
        include_timezones: wire.include_timezones,
        include_member_ids: wire.include_member_i_ds,
        include_archived: wire.include_archived,
        filter_archived: wire.filter_archived,
        only_syncable_sources: wire.only_syncable_sources,
    }
}

pub fn auth_membership_params_from_wire(
    wire: &wire::CreateDefaultMembershipParams,
) -> CreateDefaultMembershipParams {
    CreateDefaultMembershipParams {
        since: wire.since,
        re_add_removed_members: wire.re_add_removed_members,
        scoped_user_id: wire.scoped_user_id.clone(),
        scoped_team_id: wire.scoped_team_id.clone(),
        scoped_channel_id: wire.scoped_channel_id.clone(),
    }
}

impl AppPluginApi {
    /// A list read's two returns.
    fn auth_list<T, W>(
        &self,
        result: Result<Vec<T>, Box<AppError>>,
        convert: impl Fn(&T) -> W,
    ) -> (Vec<W>, Option<Box<wire::AppError>>) {
        match result {
            Ok(items) => (items.iter().map(convert).collect(), None),
            Err(err) => (Vec::new(), self.wire(err)),
        }
    }

    /// `checkLDAPLicense` for `method`: `None` when licensed, else the wired refusal.
    async fn auth_ldap_refusal(&self, method: &str) -> Option<Box<wire::AppError>> {
        let license = match self.app.license().await {
            Ok(license) => license,
            Err(err) => {
                tracing::error!(plugin_id = %self.id, error = %err.id, "the licence could not be read");
                None
            }
        };
        if auth_ldap_groups_licensed(license.as_deref()) {
            None
        } else {
            self.wire(auth_license_error(method))
        }
    }

    // -- sessions -------------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateSession` (app/plugin_api.go:335): `App.CreateSession`, whose
    /// token the plugin cannot choose. A nil session (Go dereferences it) is the zero session.
    pub(super) async fn auth_create_session(
        &self,
        args: api::Z_CreateSessionArgs,
    ) -> Result<api::Z_CreateSessionReturns, NotImplemented> {
        let session = args
            .a
            .as_deref()
            .map(auth_session_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.create_session(session).await, |s| {
            session_to_wire(&s)
        });
        Ok(api::Z_CreateSessionReturns { a, b })
    }

    /// Port of `PluginAPI.ExtendSessionExpiry` (app/plugin_api.go:339): the session by id **or
    /// token** (the store's `Get`), then its row's `ExpiresAt`, with `ExpiredNotify` cleared.
    /// Either failure is a 500 of Go's own making. Go then refreshes its session cache; here the
    /// Go process's cache for the user is cleared so it re-reads the row.
    pub(super) async fn auth_extend_session_expiry(
        &self,
        args: api::Z_ExtendSessionExpiryArgs,
    ) -> Result<api::Z_ExtendSessionExpiryReturns, NotImplemented> {
        let failed = |id: &str| {
            self.wire(AppError::boxed(
                "extendSessionExpiry",
                id,
                None,
                String::new(),
                500,
            ))
        };
        let session = match self.app.store().session().get(&args.a).await {
            Ok(session) => session,
            Err(err) => {
                tracing::debug!(error = %err, "extendSessionExpiry: no such session");
                return Ok(api::Z_ExtendSessionExpiryReturns {
                    a: failed("app.session.get_sessions.app_error"),
                });
            }
        };
        if let Err(err) = self
            .app
            .store()
            .session()
            .update_expires_at(&session.id, args.b)
            .await
        {
            tracing::error!(error = %err, "extendSessionExpiry: the update failed");
            return Ok(api::Z_ExtendSessionExpiryReturns {
                a: failed("app.session.extend_session_expiry.app_error"),
            });
        }
        if let Some(peer) = self.app.peer_cache() {
            peer.clear_user_sessions(&session.user_id).await;
        }
        Ok(api::Z_ExtendSessionExpiryReturns { a: None })
    }

    /// Port of `PluginAPI.RevokeSession` (app/plugin_api.go:352) over `App.RevokeSessionById`:
    /// a miss is 400 `app.session.get.app_error` under `RevokeSessionById`. An OAuth session is
    /// not implemented; see the module note.
    pub(super) async fn auth_revoke_session(
        &self,
        args: api::Z_RevokeSessionArgs,
    ) -> Result<api::Z_RevokeSessionReturns, NotImplemented> {
        let session = match self.app.get_session_by_id(&args.a).await {
            Ok(session) => session,
            Err(_) => {
                return Ok(api::Z_RevokeSessionReturns {
                    a: self.wire(AppError::boxed(
                        "RevokeSessionById",
                        "app.session.get.app_error",
                        None,
                        String::new(),
                        400,
                    )),
                });
            }
        };
        if session.is_oauth {
            return Err(self.not_implemented(
                "RevokeSession",
                "an OAuth session's access data has no store here",
            ));
        }
        let result = self.app.revoke_session(&session).await;
        Ok(api::Z_RevokeSessionReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- access tokens and auth data ------------------------------------------------------------

    /// Port of `PluginAPI.CreateUserAccessToken` (app/plugin_api.go:356): the token with its
    /// secret, which only this answer carries. A nil token is the zero token.
    pub(super) async fn auth_create_user_access_token(
        &self,
        args: api::Z_CreateUserAccessTokenArgs,
    ) -> Result<api::Z_CreateUserAccessTokenReturns, NotImplemented> {
        let token = args
            .a
            .as_deref()
            .map(auth_token_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.create_user_access_token(token).await, |t| {
            auth_token_to_wire(&t)
        });
        Ok(api::Z_CreateUserAccessTokenReturns { a, b })
    }

    /// Port of `PluginAPI.RevokeUserAccessToken` (app/plugin_api.go:360): read **unsanitised**
    /// (the revoke needs the secret), then revoked with the session it minted.
    pub(super) async fn auth_revoke_user_access_token(
        &self,
        args: api::Z_RevokeUserAccessTokenArgs,
    ) -> Result<api::Z_RevokeUserAccessTokenReturns, NotImplemented> {
        let result = match self.app.get_user_access_token(&args.a, false).await {
            Ok(token) => self.app.revoke_user_access_token(&token).await,
            Err(err) => Err(err),
        };
        Ok(api::Z_RevokeUserAccessTokenReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.UpdateUserAuth` (app/plugin_api.go:373): the two auth columns, a
    /// blanked password and every session revoked; answered with what was asked for.
    pub(super) async fn auth_update_user_auth(
        &self,
        args: api::Z_UpdateUserAuthArgs,
    ) -> Result<api::Z_UpdateUserAuthReturns, NotImplemented> {
        let user_auth = args
            .b
            .as_deref()
            .map(|w| UserAuth {
                auth_data: w.auth_data.clone(),
                auth_service: w.auth_service.clone(),
            })
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.update_user_auth(&args.a, &user_auth).await, |u| {
            wire::UserAuth {
                auth_data: u.auth_data,
                auth_service: u.auth_service,
            }
        });
        Ok(api::Z_UpdateUserAuthReturns { a, b })
    }

    // -- OAuth apps -----------------------------------------------------------------------------

    /// Port of `PluginAPI.CreateOAuthApp` (app/plugin_api.go:1498): `CreateOAuthAppInternal`
    /// with a secret always minted, whatever the plugin sent.
    pub(super) async fn auth_create_oauth_app(
        &self,
        args: api::Z_CreateOAuthAppArgs,
    ) -> Result<api::Z_CreateOAuthAppReturns, NotImplemented> {
        let app = args
            .a
            .as_deref()
            .map(auth_oauth_app_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(
            self.app.create_oauth_app_internal(&app, true).await,
            |app| auth_oauth_app_to_wire(&app),
        );
        Ok(api::Z_CreateOAuthAppReturns { a, b })
    }

    /// Port of `PluginAPI.GetOAuthApp` (app/plugin_api.go:1502), secret and all.
    pub(super) async fn auth_get_oauth_app(
        &self,
        args: api::Z_GetOAuthAppArgs,
    ) -> Result<api::Z_GetOAuthAppReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_oauth_app(&args.a).await, |app| {
            auth_oauth_app_to_wire(&app)
        });
        Ok(api::Z_GetOAuthAppReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateOAuthApp` (app/plugin_api.go:1506): the stored app by the
    /// plugin's id first — its miss is `GetOAuthApp`'s — then `App.UpdateOAuthApp`, which keeps
    /// the stored id, creator, creation time, secret and registration flag.
    pub(super) async fn auth_update_oauth_app(
        &self,
        args: api::Z_UpdateOAuthAppArgs,
    ) -> Result<api::Z_UpdateOAuthAppReturns, NotImplemented> {
        let updated = args
            .a
            .as_deref()
            .map(auth_oauth_app_from_wire)
            .unwrap_or_default();
        let result = match self.app.get_oauth_app(&updated.id).await {
            Ok(old) => self.app.update_oauth_app(&old, &updated).await,
            Err(err) => Err(err),
        };
        let (a, b) = self.reply(result, |app| auth_oauth_app_to_wire(&app));
        Ok(api::Z_UpdateOAuthAppReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteOAuthApp` (app/plugin_api.go:1515).
    pub(super) async fn auth_delete_oauth_app(
        &self,
        args: api::Z_DeleteOAuthAppArgs,
    ) -> Result<api::Z_DeleteOAuthAppReturns, NotImplemented> {
        let result = self.app.delete_oauth_app(&args.a).await;
        Ok(api::Z_DeleteOAuthAppReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- roles ----------------------------------------------------------------------------------

    /// Port of `PluginAPI.RolesGrantPermission` (app/plugin_api.go:1263): a live role among
    /// those named holds the permission. A lookup failure denies.
    pub(super) async fn auth_roles_grant_permission(
        &self,
        args: api::Z_RolesGrantPermissionArgs,
    ) -> Result<api::Z_RolesGrantPermissionReturns, NotImplemented> {
        Ok(api::Z_RolesGrantPermissionReturns {
            a: self.app.roles_grant_permission(&args.a, &args.b).await,
        })
    }

    // -- groups: the five ungated reads ---------------------------------------------------------

    /// Port of `PluginAPI.GetGroup` (app/plugin_api.go:783): `GetGroup(id, nil, nil)`, the row
    /// with nothing computed.
    pub(super) async fn auth_get_group(
        &self,
        args: api::Z_GetGroupArgs,
    ) -> Result<api::Z_GetGroupReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_group(&args.a).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_GetGroupReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupByName` (app/plugin_api.go:787), with empty options: a group
    /// that does not allow reference is found too.
    pub(super) async fn auth_get_group_by_name(
        &self,
        args: api::Z_GetGroupByNameArgs,
    ) -> Result<api::Z_GetGroupByNameReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_group_by_name(&args.a, false).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_GetGroupByNameReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupMemberUsers` (app/plugin_api.go:791): one page by username,
    /// sanitised as a non-admin; the count Go also reads is dropped.
    pub(super) async fn auth_get_group_member_users(
        &self,
        args: api::Z_GetGroupMemberUsersArgs,
    ) -> Result<api::Z_GetGroupMemberUsersReturns, NotImplemented> {
        let result = self
            .app
            .get_group_member_users_page(&args.a, args.b, args.c)
            .await
            .map(|(users, _)| users);
        let (a, b) = self.auth_list(result, user_to_wire);
        Ok(api::Z_GetGroupMemberUsersReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupsBySource` (app/plugin_api.go:797).
    pub(super) async fn auth_get_groups_by_source(
        &self,
        args: api::Z_GetGroupsBySourceArgs,
    ) -> Result<api::Z_GetGroupsBySourceReturns, NotImplemented> {
        let result = self
            .app
            .get_groups_by_source(&GroupSource(args.a.clone()))
            .await;
        let (a, b) = self.auth_list(result, auth_group_to_wire);
        Ok(api::Z_GetGroupsBySourceReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupsForUser` (app/plugin_api.go:801), with empty options.
    pub(super) async fn auth_get_groups_for_user(
        &self,
        args: api::Z_GetGroupsForUserArgs,
    ) -> Result<api::Z_GetGroupsForUserReturns, NotImplemented> {
        let result = self.app.get_groups_by_user_id(&args.a, false).await;
        let (a, b) = self.auth_list(result, auth_group_to_wire);
        Ok(api::Z_GetGroupsForUserReturns { a, b })
    }

    // -- groups: behind `checkLDAPLicense` ------------------------------------------------------

    /// Port of `PluginAPI.UpsertGroupMember` (app/plugin_api.go:805).
    pub(super) async fn auth_upsert_group_member(
        &self,
        args: api::Z_UpsertGroupMemberArgs,
    ) -> Result<api::Z_UpsertGroupMemberReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("UpsertGroupMember").await {
            return Ok(api::Z_UpsertGroupMemberReturns {
                a: None,
                b: Some(b),
            });
        }
        let (a, b) = self.reply(self.app.upsert_group_member(&args.a, &args.b).await, |m| {
            auth_group_member_to_wire(&m)
        });
        Ok(api::Z_UpsertGroupMemberReturns { a, b })
    }

    /// Port of `PluginAPI.UpsertGroupMembers` (app/plugin_api.go:812).
    pub(super) async fn auth_upsert_group_members(
        &self,
        args: api::Z_UpsertGroupMembersArgs,
    ) -> Result<api::Z_UpsertGroupMembersReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("UpsertGroupMembers").await {
            return Ok(api::Z_UpsertGroupMembersReturns {
                a: Vec::new(),
                b: Some(b),
            });
        }
        let result = self.app.upsert_group_members(&args.a, &args.b).await;
        let (a, b) = self.auth_list(result, auth_group_member_to_wire);
        Ok(api::Z_UpsertGroupMembersReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupByRemoteID` (app/plugin_api.go:819).
    pub(super) async fn auth_get_group_by_remote_id(
        &self,
        args: api::Z_GetGroupByRemoteIDArgs,
    ) -> Result<api::Z_GetGroupByRemoteIDReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("GetGroupByRemoteID").await {
            return Ok(api::Z_GetGroupByRemoteIDReturns {
                a: None,
                b: Some(b),
            });
        }
        let result = self
            .app
            .get_group_by_remote_id(&args.a, &GroupSource(args.b.clone()))
            .await;
        let (a, b) = self.reply(result, |g| auth_group_to_wire(&g));
        Ok(api::Z_GetGroupByRemoteIDReturns { a, b })
    }

    /// Port of `PluginAPI.CreateGroup` (app/plugin_api.go:826): the plain `CreateGroup`, which
    /// publishes nothing; see `crate::group_lookup`.
    pub(super) async fn auth_create_group(
        &self,
        args: api::Z_CreateGroupArgs,
    ) -> Result<api::Z_CreateGroupReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("CreateGroup").await {
            return Ok(api::Z_CreateGroupReturns {
                a: None,
                b: Some(b),
            });
        }
        let group = args
            .a
            .as_deref()
            .map(auth_group_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.create_group(group).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_CreateGroupReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateGroup` (app/plugin_api.go:833): the group taken whole, with its
    /// member count re-read and `received_group` published.
    pub(super) async fn auth_update_group(
        &self,
        args: api::Z_UpdateGroupArgs,
    ) -> Result<api::Z_UpdateGroupReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("UpdateGroup").await {
            return Ok(api::Z_UpdateGroupReturns {
                a: None,
                b: Some(b),
            });
        }
        let group = args
            .a
            .as_deref()
            .map(auth_group_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.update_group(group).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_UpdateGroupReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteGroup` (app/plugin_api.go:840).
    pub(super) async fn auth_delete_group(
        &self,
        args: api::Z_DeleteGroupArgs,
    ) -> Result<api::Z_DeleteGroupReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("DeleteGroup").await {
            return Ok(api::Z_DeleteGroupReturns {
                a: None,
                b: Some(b),
            });
        }
        let (a, b) = self.reply(self.app.delete_group(&args.a).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_DeleteGroupReturns { a, b })
    }

    /// Port of `PluginAPI.RestoreGroup` (app/plugin_api.go:847).
    pub(super) async fn auth_restore_group(
        &self,
        args: api::Z_RestoreGroupArgs,
    ) -> Result<api::Z_RestoreGroupReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("RestoreGroup").await {
            return Ok(api::Z_RestoreGroupReturns {
                a: None,
                b: Some(b),
            });
        }
        let (a, b) = self.reply(self.app.restore_group(&args.a).await, |g| {
            auth_group_to_wire(&g)
        });
        Ok(api::Z_RestoreGroupReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteGroupMember` (app/plugin_api.go:854).
    pub(super) async fn auth_delete_group_member(
        &self,
        args: api::Z_DeleteGroupMemberArgs,
    ) -> Result<api::Z_DeleteGroupMemberReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("DeleteGroupMember").await {
            return Ok(api::Z_DeleteGroupMemberReturns {
                a: None,
                b: Some(b),
            });
        }
        let (a, b) = self.reply(self.app.delete_group_member(&args.a, &args.b).await, |m| {
            auth_group_member_to_wire(&m)
        });
        Ok(api::Z_DeleteGroupMemberReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupSyncable` (app/plugin_api.go:861): the row whatever its
    /// `DeleteAt`.
    pub(super) async fn auth_get_group_syncable(
        &self,
        args: api::Z_GetGroupSyncableArgs,
    ) -> Result<api::Z_GetGroupSyncableReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("GetGroupSyncable").await {
            return Ok(api::Z_GetGroupSyncableReturns {
                a: None,
                b: Some(b),
            });
        }
        let result = self
            .app
            .get_group_syncable(&args.a, &args.b, &GroupSyncableType(args.c.clone()))
            .await;
        let (a, b) = self.reply(result, |gs| auth_group_syncable_to_wire(&gs));
        Ok(api::Z_GetGroupSyncableReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroupSyncables` (app/plugin_api.go:868).
    pub(super) async fn auth_get_group_syncables(
        &self,
        args: api::Z_GetGroupSyncablesArgs,
    ) -> Result<api::Z_GetGroupSyncablesReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("GetGroupSyncables").await {
            return Ok(api::Z_GetGroupSyncablesReturns {
                a: Vec::new(),
                b: Some(b),
            });
        }
        let result = self
            .app
            .get_group_syncables(&args.a, &GroupSyncableType(args.b.clone()))
            .await;
        let (a, b) = self.auth_list(result, auth_group_syncable_to_wire);
        Ok(api::Z_GetGroupSyncablesReturns { a, b })
    }

    /// Port of `PluginAPI.UpsertGroupSyncable` (app/plugin_api.go:875): a channel link links
    /// the channel's team first, as the REST link does.
    pub(super) async fn auth_upsert_group_syncable(
        &self,
        args: api::Z_UpsertGroupSyncableArgs,
    ) -> Result<api::Z_UpsertGroupSyncableReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("UpsertGroupSyncable").await {
            return Ok(api::Z_UpsertGroupSyncableReturns {
                a: None,
                b: Some(b),
            });
        }
        let gs = args
            .a
            .as_deref()
            .map(auth_group_syncable_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.upsert_group_syncable(gs).await, |gs| {
            auth_group_syncable_to_wire(&gs)
        });
        Ok(api::Z_UpsertGroupSyncableReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateGroupSyncable` (app/plugin_api.go:882).
    pub(super) async fn auth_update_group_syncable(
        &self,
        args: api::Z_UpdateGroupSyncableArgs,
    ) -> Result<api::Z_UpdateGroupSyncableReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("UpdateGroupSyncable").await {
            return Ok(api::Z_UpdateGroupSyncableReturns {
                a: None,
                b: Some(b),
            });
        }
        let gs = args
            .a
            .as_deref()
            .map(auth_group_syncable_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.update_group_syncable(gs).await, |gs| {
            auth_group_syncable_to_wire(&gs)
        });
        Ok(api::Z_UpdateGroupSyncableReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteGroupSyncable` (app/plugin_api.go:889): a team link takes every
    /// channel link of the group with it.
    pub(super) async fn auth_delete_group_syncable(
        &self,
        args: api::Z_DeleteGroupSyncableArgs,
    ) -> Result<api::Z_DeleteGroupSyncableReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("DeleteGroupSyncable").await {
            return Ok(api::Z_DeleteGroupSyncableReturns {
                a: None,
                b: Some(b),
            });
        }
        let result = self
            .app
            .delete_group_syncable(&args.a, &args.b, &GroupSyncableType(args.c.clone()))
            .await;
        let (a, b) = self.reply(result, |gs| auth_group_syncable_to_wire(&gs));
        Ok(api::Z_DeleteGroupSyncableReturns { a, b })
    }

    /// Port of `PluginAPI.GetGroups` (app/plugin_api.go:1669).
    pub(super) async fn auth_get_groups(
        &self,
        args: api::Z_GetGroupsArgs,
    ) -> Result<api::Z_GetGroupsReturns, NotImplemented> {
        if let Some(b) = self.auth_ldap_refusal("GetGroups").await {
            return Ok(api::Z_GetGroupsReturns {
                a: Vec::new(),
                b: Some(b),
            });
        }
        let opts = auth_group_search_opts_from_wire(&args.c);
        let restrictions = args.d.as_deref().map(|r| ViewUsersRestrictions {
            teams: r.teams.clone(),
            channels: r.channels.clone(),
        });
        let result = self
            .app
            .get_groups(args.a, args.b, &opts, restrictions.as_ref())
            .await;
        let (a, b) = self.auth_list(result, auth_group_to_wire);
        Ok(api::Z_GetGroupsReturns { a, b })
    }

    /// Port of `PluginAPI.CreateDefaultSyncableMemberships` (app/plugin_api.go:1676): the
    /// memberships the auto-add links call for; any failure is one 500 whose cause stays behind.
    pub(super) async fn auth_create_default_syncable_memberships(
        &self,
        args: api::Z_CreateDefaultSyncableMembershipsArgs,
    ) -> Result<api::Z_CreateDefaultSyncableMembershipsReturns, NotImplemented> {
        if let Some(a) = self
            .auth_ldap_refusal("CreateDefaultSyncableMemberships")
            .await
        {
            return Ok(api::Z_CreateDefaultSyncableMembershipsReturns { a: Some(a) });
        }
        let params = auth_membership_params_from_wire(&args.a);
        let a = match self
            .app
            .create_default_memberships(&params, &HookContext::default())
            .await
        {
            Ok(()) => None,
            Err(cause) => {
                tracing::warn!(plugin_id = %self.id, cause, "CreateDefaultSyncableMemberships failed");
                self.wire(AppError::boxed(
                    "CreateDefaultSyncableMemberships",
                    "app.group.create_syncable_memberships.error",
                    None,
                    String::new(),
                    500,
                ))
            }
        };
        Ok(api::Z_CreateDefaultSyncableMembershipsReturns { a })
    }

    /// Port of `PluginAPI.DeleteGroupConstrainedMemberships` (app/plugin_api.go:1689): over the
    /// whole installation; see `App::delete_group_constrained_memberships`.
    pub(super) async fn auth_delete_group_constrained_memberships(
        &self,
    ) -> Result<api::Z_DeleteGroupConstrainedMembershipsReturns, NotImplemented> {
        if let Some(a) = self
            .auth_ldap_refusal("DeleteGroupConstrainedMemberships")
            .await
        {
            return Ok(api::Z_DeleteGroupConstrainedMembershipsReturns { a: Some(a) });
        }
        let a = match self
            .app
            .delete_group_constrained_memberships(&HookContext::default())
            .await
        {
            Ok(()) => None,
            Err(cause) => {
                tracing::warn!(plugin_id = %self.id, cause, "DeleteGroupConstrainedMemberships failed");
                self.wire(AppError::boxed(
                    "DeleteGroupConstrainedMemberships",
                    "app.group.delete_invalid_syncable_memberships.error",
                    None,
                    String::new(),
                    500,
                ))
            }
        };
        Ok(api::Z_DeleteGroupConstrainedMembershipsReturns { a })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::license::{Features, License};

    fn licence(ldap_groups: Option<bool>) -> License {
        License {
            features: Some(Features {
                ldap_groups,
                ..Features::default()
            }),
            ..License::default()
        }
    }

    #[test]
    fn the_ldap_gate_wants_a_licence_with_ldap_groups() {
        assert!(!auth_ldap_groups_licensed(None), "no licence");
        assert!(
            !auth_ldap_groups_licensed(Some(&licence(Some(false)))),
            "LDAPGroups off"
        );
        assert!(
            !auth_ldap_groups_licensed(Some(&License::default())),
            "no features at all"
        );
        assert!(auth_ldap_groups_licensed(Some(&licence(Some(true)))));
    }

    #[test]
    fn the_licence_refusal_is_a_403_under_the_method_name() {
        let err = auth_license_error("CreateGroup");
        assert_eq!(
            (err.id.as_str(), err.where_.as_str(), err.status_code),
            ("app.group.license_error", "CreateGroup", 403)
        );
    }

    #[test]
    fn a_group_crosses_both_ways_and_an_empty_member_list_is_nil() {
        let group = Group {
            id: "g".into(),
            name: Some("n".into()),
            display_name: "d".into(),
            description: "x".into(),
            source: GroupSource("plugin_a".into()),
            remote_id: Some("r".into()),
            create_at: 1,
            update_at: 2,
            delete_at: 3,
            has_syncables: true,
            member_count: Some(4),
            allow_reference: true,
            channel_member_count: Some(5),
            channel_member_timezones_count: Some(6),
            member_ids: Some(vec!["u".into()]),
        };
        let wire = auth_group_to_wire(&group);
        assert_eq!(wire.source, "plugin_a");
        assert_eq!(wire.member_i_ds, vec!["u".to_owned()]);
        assert_eq!(auth_group_from_wire(&wire), group);
        let empty = auth_group_from_wire(&wire::Group::default());
        assert_eq!(empty.member_ids, None);
        assert_eq!(empty.name, None);
    }

    #[test]
    fn an_oauth_app_keeps_its_secret_and_an_empty_callback_list_is_nil() {
        let app = OAuthApp {
            id: "a".into(),
            client_secret: "s".into(),
            callback_urls: Some(vec!["https://x".into()]),
            ..OAuthApp::default()
        };
        let wire = auth_oauth_app_to_wire(&app);
        assert_eq!(wire.client_secret, "s");
        assert_eq!(auth_oauth_app_from_wire(&wire), app);
        assert_eq!(
            auth_oauth_app_from_wire(&wire::OAuthApp::default()).callback_urls,
            None
        );
    }

    #[test]
    fn a_session_from_a_plugin_drops_its_team_members_and_keeps_its_props() {
        let mut wire = wire::Session {
            user_id: "u".into(),
            is_o_auth: true,
            team_members: vec![wire::TeamMember::default()],
            ..wire::Session::default()
        };
        wire.props.insert("k".into(), "v".into());
        let session = auth_session_from_wire(&wire);
        assert!(session.is_oauth);
        assert_eq!(session.team_members, None);
        assert_eq!(
            session.props.as_ref().and_then(|p| p.get("k")).cloned(),
            Some("v".to_owned())
        );
        assert_eq!(
            auth_session_from_wire(&wire::Session::default()).props,
            None
        );
    }

    #[test]
    fn the_search_options_carry_every_field_the_store_reads() {
        let wire = wire::GroupSearchOpts {
            q: "q".into(),
            include_member_count: true,
            include_member_i_ds: true,
            source: "plugin_s".into(),
            page_opts: Some(Box::new(wire::PageOpts {
                page: 2,
                per_page: 3,
            })),
            filter_archived: true,
            ..wire::GroupSearchOpts::default()
        };
        let opts = auth_group_search_opts_from_wire(&wire);
        assert_eq!(opts.q, "q");
        assert!(opts.include_member_count && opts.include_member_ids && opts.filter_archived);
        assert_eq!(opts.source.as_str(), "plugin_s");
        assert_eq!(opts.page_opts.map(|p| (p.page, p.per_page)), Some((2, 3)));
    }
}
