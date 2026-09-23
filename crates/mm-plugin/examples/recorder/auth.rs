//! The hook recorder's auth script: the plugin API's session, access-token, auth-data, OAuth-app,
//! role and group methods, each written down with what the host answered, for
//! `parity::plugin_hooks`' auth tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`AUTH_SCRIPT`], from inside `MessageWillBePosted`, like the
//! channels script. The suite runs it twice under each host: once unlicensed, where every group
//! method behind `checkLDAPLicense` refuses, and once with a licence, where they write.
//!
//! # What the suite hands it
//!
//! Through the environment: this side's **own** user and its username, its **other** user, an
//! **outsider** (made on the side team and removed from it), the **side team**, the **side** tag,
//! and the **phase** (`unlicensed` or `licensed`). The own channel is the trigger's.
//!
//! Everything the script makes it also undoes — a session revoked, a token revoked, an OAuth app
//! deleted, a group deleted — except what a licensed run leaves on purpose: the outsider back on
//! the team, and the other user moved to `gitlab`. The suite removes all of it afterwards.

use go_netrpc::Client;
use mm_plugin::wire::model::{
    CreateDefaultMembershipParams, Group, GroupSearchOpts, GroupSyncable, OAuthApp, Session,
    UserAccessToken, UserAuth, ViewUsersRestrictions,
};
use mm_plugin::wire::plugin::*;
use serde_json::Value as Json;
use std::collections::HashMap;

use crate::core::{MISSING, call};

/// The message that runs the script.
pub const AUTH_SCRIPT: &str = "!auth-script";

/// A fixed expiry the suite recognises after the masks: 2100-01-01.
pub const FAR: i64 = 4_102_444_800_000;

/// What the suite put in the environment, and the trigger's channel.
pub struct Inputs {
    pub own: String,
    pub own_name: String,
    pub other: String,
    pub outsider: String,
    pub side_team: String,
    pub side: String,
    pub licensed: bool,
    pub own_channel: String,
}

impl Inputs {
    /// The environment's half, with the trigger's channel.
    pub fn from_env(own_channel: &str) -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            own: var("HOOK_RECORDER_AUTH_OWN"),
            own_name: var("HOOK_RECORDER_AUTH_OWN_NAME"),
            other: var("HOOK_RECORDER_AUTH_OTHER"),
            outsider: var("HOOK_RECORDER_AUTH_OUTSIDER"),
            side_team: var("HOOK_RECORDER_AUTH_TEAM"),
            side: var("HOOK_RECORDER_AUTH_SIDE"),
            licensed: var("HOOK_RECORDER_AUTH_PHASE") == "licensed",
            own_channel: own_channel.to_owned(),
        }
    }

    /// This side's plugin group source.
    fn source(&self) -> String {
        format!("plugin_{}", self.side)
    }
}

async fn get_session(api: &Client, out: &mut Vec<Json>, id: &str) {
    let _: Option<Z_GetSessionReturns> = call(
        api,
        out,
        "GetSession",
        Z_GetSessionArgs { a: id.to_owned() },
    )
    .await;
}

/// Sessions: made (for a user, for nobody, refused twice), read, extended by id and by token,
/// and revoked.
async fn sessions(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let props: HashMap<String, String> = [("platform".to_owned(), "plug".to_owned())].into();
    let made: Option<Z_CreateSessionReturns> = call(
        api,
        out,
        "CreateSession",
        Z_CreateSessionArgs {
            a: Some(Box::new(Session {
                user_id: input.other.clone(),
                roles: "system_user".into(),
                device_id: format!("plugdev-{}", input.side),
                // Discarded: the platform blanks it before the store mints one.
                token: "chosenbytheplugin".into(),
                props,
                ..Session::default()
            })),
        },
    )
    .await;
    let session = made.and_then(|r| r.a).map(|s| *s).unwrap_or_default();
    // For a user that does not exist: allowed, "some unit tests rely on it".
    let nobody: Option<Z_CreateSessionReturns> = call(
        api,
        out,
        "CreateSession",
        Z_CreateSessionArgs {
            a: Some(Box::new(Session {
                user_id: MISSING.into(),
                device_id: format!("plugdev-{}", input.side),
                ..Session::default()
            })),
        },
    )
    .await;
    let nobody = nobody.and_then(|r| r.a).map(|s| s.id).unwrap_or_default();
    // An id already set, then a user id that is not one.
    for (id, user) in [(MISSING, input.other.as_str()), ("", "notanid")] {
        let _: Option<Z_CreateSessionReturns> = call(
            api,
            out,
            "CreateSession",
            Z_CreateSessionArgs {
                a: Some(Box::new(Session {
                    id: id.to_owned(),
                    user_id: user.to_owned(),
                    ..Session::default()
                })),
            },
        )
        .await;
    }
    get_session(api, out, &session.id).await;
    // By id, then by token (the store's `Get` takes either), then nothing.
    for (key, expiry) in [
        (session.id.as_str(), FAR),
        (session.token.as_str(), FAR + 1000),
        (MISSING, FAR),
    ] {
        let _: Option<Z_ExtendSessionExpiryReturns> = call(
            api,
            out,
            "ExtendSessionExpiry",
            Z_ExtendSessionExpiryArgs {
                a: key.to_owned(),
                b: expiry,
            },
        )
        .await;
        get_session(api, out, &session.id).await;
    }
    for id in [session.id.as_str(), session.id.as_str(), nobody.as_str()] {
        let _: Option<Z_RevokeSessionReturns> = call(
            api,
            out,
            "RevokeSession",
            Z_RevokeSessionArgs { a: id.to_owned() },
        )
        .await;
    }
    get_session(api, out, &session.id).await;
}

/// User access tokens: one made and revoked twice; three refusals.
async fn tokens(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let made: Option<Z_CreateUserAccessTokenReturns> = call(
        api,
        out,
        "CreateUserAccessToken",
        Z_CreateUserAccessTokenArgs {
            a: Some(Box::new(UserAccessToken {
                // Both discarded: the store mints the id, the app the secret.
                id: MISSING.into(),
                token: "chosenbytheplugin".into(),
                user_id: input.own.clone(),
                description: format!("plugin token {}", input.side),
                expires_at: FAR,
                ..UserAccessToken::default()
            })),
        },
    )
    .await;
    let token = made.and_then(|r| r.a).map(|t| t.id).unwrap_or_default();
    // A user that is not there; an expiry already past.
    for (user, expires_at) in [(MISSING, FAR), (input.own.as_str(), 1)] {
        let _: Option<Z_CreateUserAccessTokenReturns> = call(
            api,
            out,
            "CreateUserAccessToken",
            Z_CreateUserAccessTokenArgs {
                a: Some(Box::new(UserAccessToken {
                    user_id: user.to_owned(),
                    description: "refused".into(),
                    expires_at,
                    ..UserAccessToken::default()
                })),
            },
        )
        .await;
    }
    for id in [token.as_str(), token.as_str(), MISSING] {
        let _: Option<Z_RevokeUserAccessTokenReturns> = call(
            api,
            out,
            "RevokeUserAccessToken",
            Z_RevokeUserAccessTokenArgs { a: id.to_owned() },
        )
        .await;
    }
}

/// OAuth apps: one made, read, updated, deleted; each refusal.
async fn oauth_apps(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let app = |name: String| OAuthApp {
        creator_id: input.own.clone(),
        name,
        description: "made by a plugin".into(),
        callback_urls: vec!["https://example.com/callback".into()],
        homepage: "https://example.com".into(),
        // Replaced: `CreateOAuthApp` always mints one.
        client_secret: "chosenbytheplugin".into(),
        ..OAuthApp::default()
    };
    let made: Option<Z_CreateOAuthAppReturns> = call(
        api,
        out,
        "CreateOAuthApp",
        Z_CreateOAuthAppArgs {
            a: Some(Box::new(app(format!("plugapp {}", input.side)))),
        },
    )
    .await;
    let id = made.and_then(|r| r.a).map(|a| a.id).unwrap_or_default();
    // An id already set; no name.
    for (preset, name) in [(MISSING, "plugapp preset"), ("", "")] {
        let _: Option<Z_CreateOAuthAppReturns> = call(
            api,
            out,
            "CreateOAuthApp",
            Z_CreateOAuthAppArgs {
                a: Some(Box::new(OAuthApp {
                    id: preset.to_owned(),
                    ..app(name.to_owned())
                })),
            },
        )
        .await;
    }
    for key in [id.as_str(), MISSING] {
        let _: Option<Z_GetOAuthAppReturns> = call(
            api,
            out,
            "GetOAuthApp",
            Z_GetOAuthAppArgs { a: key.to_owned() },
        )
        .await;
    }
    // The creator, the secret and the creation time are the stored app's, whatever is sent;
    // then a missing app, then an invalid one.
    for (key, name) in [
        (id.as_str(), format!("plugapp renamed {}", input.side)),
        (MISSING, "plugapp missing".to_owned()),
        (id.as_str(), String::new()),
    ] {
        let _: Option<Z_UpdateOAuthAppReturns> = call(
            api,
            out,
            "UpdateOAuthApp",
            Z_UpdateOAuthAppArgs {
                a: Some(Box::new(OAuthApp {
                    id: key.to_owned(),
                    creator_id: input.other.clone(),
                    create_at: 1,
                    client_secret: "rotatedbytheplugin".into(),
                    is_trusted: true,
                    callback_urls: vec![
                        "https://example.com/one".into(),
                        "https://example.com/two".into(),
                    ],
                    ..app(name)
                })),
            },
        )
        .await;
    }
    for key in [id.as_str(), MISSING] {
        let _: Option<Z_DeleteOAuthAppReturns> = call(
            api,
            out,
            "DeleteOAuthApp",
            Z_DeleteOAuthAppArgs { a: key.to_owned() },
        )
        .await;
    }
    let _: Option<Z_GetOAuthAppReturns> =
        call(api, out, "GetOAuthApp", Z_GetOAuthAppArgs { a: id }).await;
}

/// Role grants: a stock grant, a stock refusal, an admin's grant, a role that is not there, and
/// no role at all.
async fn roles(api: &Client, out: &mut Vec<Json>) {
    for (names, permission) in [
        (vec!["system_user"], "create_team"),
        (vec!["system_user"], "manage_system"),
        (vec!["system_user", "system_admin"], "manage_system"),
        (vec!["nosuchpluginrole"], "create_team"),
        (vec![], "create_team"),
    ] {
        let _: Option<Z_RolesGrantPermissionReturns> = call(
            api,
            out,
            "RolesGrantPermission",
            Z_RolesGrantPermissionArgs {
                a: names.into_iter().map(str::to_owned).collect(),
                b: permission.to_owned(),
            },
        )
        .await;
    }
}

/// Auth data: the other user moved to `gitlab` (its sessions revoked), read back; the same data
/// for the outsider (a unique violation); and a user that is not there (nothing matched, a
/// success).
async fn auth_data(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for user in [input.other.as_str(), input.outsider.as_str(), MISSING] {
        let _: Option<Z_UpdateUserAuthReturns> = call(
            api,
            out,
            "UpdateUserAuth",
            Z_UpdateUserAuthArgs {
                a: user.to_owned(),
                b: Some(Box::new(UserAuth {
                    auth_data: Some(format!("plugauth-{}", input.side)),
                    auth_service: "gitlab".into(),
                })),
            },
        )
        .await;
    }
    let _: Option<Z_GetUserReturns> = call(
        api,
        out,
        "GetUser",
        Z_GetUserArgs {
            a: input.other.clone(),
        },
    )
    .await;
}

/// The group this side makes, with what `CreateGroup` must echo back unchanged.
fn group(input: &Inputs) -> Group {
    Group {
        name: Some(format!("plug-{}", input.side)),
        display_name: format!("Plug {}", input.side),
        description: "made by a plugin".into(),
        source: input.source(),
        remote_id: Some(format!("remote-{}", input.side)),
        // Off, so that a read which filtered on it would miss the group: `GetGroupByName` and
        // `GetGroupsForUser` pass empty options, and only `GetGroups` below asks for the filter.
        allow_reference: false,
        // Answered as sent: `Create` reads nothing back.
        member_count: Some(7),
        ..Group::default()
    }
}

/// A group made and refused, read four ways, filled, paged, listed, linked, synced, emptied,
/// deleted and restored.
async fn groups(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let _: Option<Z_GetGroupReturns> =
        call(api, out, "GetGroup", Z_GetGroupArgs { a: MISSING.into() }).await;
    let made: Option<Z_CreateGroupReturns> = call(
        api,
        out,
        "CreateGroup",
        Z_CreateGroupArgs {
            a: Some(Box::new(group(input))),
        },
    )
    .await;
    let made = made.and_then(|r| r.a).map(|g| *g).unwrap_or_default();
    let id = made.id.clone();
    // An id already set; a username; no display name; a duplicate name.
    for refused in [
        Group {
            id: MISSING.into(),
            ..group(input)
        },
        Group {
            name: Some(input.own_name.clone()),
            ..group(input)
        },
        Group {
            display_name: String::new(),
            ..group(input)
        },
        Group {
            remote_id: Some(format!("remote2-{}", input.side)),
            ..group(input)
        },
    ] {
        let _: Option<Z_CreateGroupReturns> = call(
            api,
            out,
            "CreateGroup",
            Z_CreateGroupArgs {
                a: Some(Box::new(refused)),
            },
        )
        .await;
    }
    let _: Option<Z_GetGroupReturns> =
        call(api, out, "GetGroup", Z_GetGroupArgs { a: id.clone() }).await;
    for name in [
        format!("plug-{}", input.side),
        "nosuchplugingroup".to_owned(),
    ] {
        let _: Option<Z_GetGroupByNameReturns> =
            call(api, out, "GetGroupByName", Z_GetGroupByNameArgs { a: name }).await;
    }
    for source in [input.source(), "plugin_nosuchsource".to_owned()] {
        let _: Option<Z_GetGroupByRemoteIDReturns> = call(
            api,
            out,
            "GetGroupByRemoteID",
            Z_GetGroupByRemoteIDArgs {
                a: format!("remote-{}", input.side),
                b: source,
            },
        )
        .await;
    }

    // Members.
    for (group_id, user) in [
        (id.as_str(), input.own.as_str()),
        (MISSING, input.own.as_str()),
    ] {
        let _: Option<Z_UpsertGroupMemberReturns> = call(
            api,
            out,
            "UpsertGroupMember",
            Z_UpsertGroupMemberArgs {
                a: group_id.to_owned(),
                b: user.to_owned(),
            },
        )
        .await;
    }
    for users in [
        vec![input.other.clone(), input.outsider.clone()],
        vec![MISSING.to_owned()],
        vec![],
    ] {
        let _: Option<Z_UpsertGroupMembersReturns> = call(
            api,
            out,
            "UpsertGroupMembers",
            Z_UpsertGroupMembersArgs {
                a: id.clone(),
                b: users,
            },
        )
        .await;
    }
    // By username; page 1 of 2 is the third (offset `page * perPage`); a negative page size is
    // Postgres' refusal; a group that is not there has no members.
    for (group_id, page, per_page) in [
        (id.as_str(), 0, 100),
        (id.as_str(), 1, 2),
        (id.as_str(), 0, -1),
        (MISSING, 0, 100),
    ] {
        let _: Option<Z_GetGroupMemberUsersReturns> = call(
            api,
            out,
            "GetGroupMemberUsers",
            Z_GetGroupMemberUsersArgs {
                a: group_id.to_owned(),
                b: page,
                c: per_page,
            },
        )
        .await;
    }
    for user in [input.own.as_str(), MISSING] {
        let _: Option<Z_GetGroupsForUserReturns> = call(
            api,
            out,
            "GetGroupsForUser",
            Z_GetGroupsForUserArgs { a: user.to_owned() },
        )
        .await;
    }
    let _: Option<Z_GetGroupsBySourceReturns> = call(
        api,
        out,
        "GetGroupsBySource",
        Z_GetGroupsBySourceArgs { a: input.source() },
    )
    .await;

    // Updates: the display name; a group that is not there; a username as the name.
    for (key, name, display) in [
        (id.as_str(), format!("plug-{}", input.side), "Plug renamed"),
        (MISSING, "plug-missing".to_owned(), "Missing"),
        (id.as_str(), input.own_name.clone(), "Plug user"),
    ] {
        let _: Option<Z_UpdateGroupReturns> = call(
            api,
            out,
            "UpdateGroup",
            Z_UpdateGroupArgs {
                a: Some(Box::new(Group {
                    id: key.to_owned(),
                    name: Some(name),
                    display_name: display.to_owned(),
                    create_at: made.create_at,
                    ..group(input)
                })),
            },
        )
        .await;
    }
    for (opts, restricted) in [
        (
            GroupSearchOpts {
                source: input.source(),
                include_member_count: true,
                include_member_i_ds: true,
                ..GroupSearchOpts::default()
            },
            false,
        ),
        (
            GroupSearchOpts {
                q: format!("plug-{}", input.side),
                include_member_count: true,
                ..GroupSearchOpts::default()
            },
            true,
        ),
        (
            GroupSearchOpts {
                source: input.source(),
                filter_allow_reference: true,
                filter_has_member: input.other.clone(),
                ..GroupSearchOpts::default()
            },
            false,
        ),
    ] {
        let _: Option<Z_GetGroupsReturns> = call(
            api,
            out,
            "GetGroups",
            Z_GetGroupsArgs {
                a: 0,
                b: 100,
                c: opts,
                d: restricted.then(|| {
                    Box::new(ViewUsersRestrictions {
                        teams: vec![input.side_team.clone()],
                        channels: vec![],
                    })
                }),
            },
        )
        .await;
    }

    // Links. The team first, and the outsider is synced back onto it before the channel link
    // (which re-links the team with its own `AutoAdd`) turns auto-add off.
    let link = |syncable: &str, kind: &str, auto_add: bool| GroupSyncable {
        group_id: id.clone(),
        syncable_id: syncable.to_owned(),
        r#type: kind.to_owned(),
        auto_add,
        ..GroupSyncable::default()
    };
    let _: Option<Z_UpsertGroupSyncableReturns> = call(
        api,
        out,
        "UpsertGroupSyncable",
        Z_UpsertGroupSyncableArgs {
            a: Some(Box::new(link(&input.side_team, "Team", true))),
        },
    )
    .await;
    let _: Option<Z_GetGroupSyncableReturns> = call(
        api,
        out,
        "GetGroupSyncable",
        Z_GetGroupSyncableArgs {
            a: id.clone(),
            b: input.side_team.clone(),
            c: "Team".into(),
        },
    )
    .await;
    let _: Option<Z_CreateDefaultSyncableMembershipsReturns> = call(
        api,
        out,
        "CreateDefaultSyncableMemberships",
        Z_CreateDefaultSyncableMembershipsArgs {
            a: CreateDefaultMembershipParams {
                since: 0,
                re_add_removed_members: true,
                scoped_team_id: Some(input.side_team.clone()),
                ..CreateDefaultMembershipParams::default()
            },
        },
    )
    .await;
    for (syncable, kind) in [
        (input.own_channel.as_str(), "Channel"),
        (MISSING, "Channel"),
    ] {
        let _: Option<Z_UpsertGroupSyncableReturns> = call(
            api,
            out,
            "UpsertGroupSyncable",
            Z_UpsertGroupSyncableArgs {
                a: Some(Box::new(link(syncable, kind, false))),
            },
        )
        .await;
    }
    for kind in ["Channel", "Team"] {
        let _: Option<Z_GetGroupSyncablesReturns> = call(
            api,
            out,
            "GetGroupSyncables",
            Z_GetGroupSyncablesArgs {
                a: id.clone(),
                b: kind.into(),
            },
        )
        .await;
    }
    let _: Option<Z_UpdateGroupSyncableReturns> = call(
        api,
        out,
        "UpdateGroupSyncable",
        Z_UpdateGroupSyncableArgs {
            a: Some(Box::new(GroupSyncable {
                scheme_admin: true,
                ..link(&input.own_channel, "Channel", false)
            })),
        },
    )
    .await;
    // The team link takes the channel's with it; then each is already gone, or never was.
    for (syncable, kind) in [
        (input.side_team.as_str(), "Team"),
        (input.own_channel.as_str(), "Channel"),
        (MISSING, "Team"),
    ] {
        let _: Option<Z_DeleteGroupSyncableReturns> = call(
            api,
            out,
            "DeleteGroupSyncable",
            Z_DeleteGroupSyncableArgs {
                a: id.clone(),
                b: syncable.to_owned(),
                c: kind.into(),
            },
        )
        .await;
    }
    let _: Option<Z_GetGroupSyncableReturns> = call(
        api,
        out,
        "GetGroupSyncable",
        Z_GetGroupSyncableArgs {
            a: id.clone(),
            b: input.own_channel.clone(),
            c: "Channel".into(),
        },
    )
    .await;

    // Emptied, deleted, restored, deleted again.
    for _ in 0..2 {
        let _: Option<Z_DeleteGroupMemberReturns> = call(
            api,
            out,
            "DeleteGroupMember",
            Z_DeleteGroupMemberArgs {
                a: id.clone(),
                b: input.outsider.clone(),
            },
        )
        .await;
    }
    for name in [
        "DeleteGroup",
        "DeleteGroup",
        "RestoreGroup",
        "RestoreGroup",
        "DeleteGroup",
    ] {
        if name == "DeleteGroup" {
            let _: Option<Z_DeleteGroupReturns> =
                call(api, out, name, Z_DeleteGroupArgs { a: id.clone() }).await;
        } else {
            let _: Option<Z_RestoreGroupReturns> =
                call(api, out, name, Z_RestoreGroupArgs { a: id.clone() }).await;
        }
    }
    // `GetByUser` tests the membership's `DeleteAt`, not the group's: the deleted group is
    // still the own user's.
    let _: Option<Z_GetGroupsForUserReturns> = call(
        api,
        out,
        "GetGroupsForUser",
        Z_GetGroupsForUserArgs {
            a: input.own.clone(),
        },
    )
    .await;
    // Over the whole installation, so only where it is refused.
    if !input.licensed {
        let _: Option<Z_DeleteGroupConstrainedMembershipsReturns> = call(
            api,
            out,
            "DeleteGroupConstrainedMemberships",
            Z_DeleteGroupConstrainedMembershipsArgs {},
        )
        .await;
    }
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    sessions(api, input, &mut out).await;
    tokens(api, input, &mut out).await;
    oauth_apps(api, input, &mut out).await;
    roles(api, &mut out).await;
    groups(api, input, &mut out).await;
    auth_data(api, input, &mut out).await;
    out
}
