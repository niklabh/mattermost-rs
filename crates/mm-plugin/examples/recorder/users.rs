//! The hook recorder's users script: the plugin API's user, status, preference and team methods,
//! each written down with what the host answered, for `parity::plugin_hooks`' users tranche
//! (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`USERS_SCRIPT`], from inside `MessageWillBePosted`, like the
//! core script, and writes through the API as a plugin would: a user it creates, deactivates,
//! reactivates and deletes; its own user's profile, roles, custom status, statuses and
//! preferences; and a team it creates, fills, edits and archives. Every hook those writes fire
//! lands in the transcript beside the script's entry.
//!
//! # What the suite hands it
//!
//! Through the environment: a **reader** both sides may read and neither writes, the **shared
//! team** it belongs to, and this side's **own** user, **other** user and **side team**, with a
//! **side** tag that every name the script makes carries. The own channel is the trigger's.
//!
//! # Two reads are filtered here
//!
//! `GetUsers` and `GetTeams` answer the whole installation, which other suites change while this
//! one runs. Their answers are written down with only the users and teams this script knows,
//! **in the order the host gave them**, so the order is still compared.

use go_netrpc::Client;
use mm_plugin::wire::model::{CustomStatus, Preference, Team, User, UserGetOptions, UserSearch};
use mm_plugin::wire::plugin::*;
use serde_json::{Value as Json, json};

use crate::core::{MISSING, call};
use crate::render::render_typed;

/// The message that runs the script.
pub const USERS_SCRIPT: &str = "!users-script";

/// The password the script's own user is created with; it satisfies the default policy.
const PASSWORD: &str = "Mmrs-Plugin-Users-1234";

/// What the suite put in the environment, and the trigger's channel.
pub struct Inputs {
    pub reader: String,
    pub shared_team: String,
    pub own: String,
    pub other: String,
    pub side_team: String,
    pub side: String,
    pub own_channel: String,
}

impl Inputs {
    /// The environment's half, with the trigger's channel.
    pub fn from_env(own_channel: &str) -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            reader: var("HOOK_RECORDER_USERS_READER"),
            shared_team: var("HOOK_RECORDER_USERS_SHARED_TEAM"),
            own: var("HOOK_RECORDER_USERS_OWN"),
            other: var("HOOK_RECORDER_USERS_OTHER"),
            side_team: var("HOOK_RECORDER_USERS_TEAM"),
            side: var("HOOK_RECORDER_USERS_SIDE"),
            own_channel: own_channel.to_owned(),
        }
    }
}

/// [`call`] for a read of the whole installation: the answer's `A` is written down with only the
/// elements whose `Id` is in `known`, in the host's order.
async fn call_filtered<A, R>(api: &Client, out: &mut Vec<Json>, name: &str, args: A, known: &[&str])
where
    A: gobwire::Encode,
    R: gobwire::Decode + Default + gobwire::Encode + Send + 'static,
{
    match api.call::<A, R>(&format!("Plugin.{name}"), &args).await {
        Ok(returns) => {
            let mut rendered = render_typed(&returns);
            if let Some(items) = rendered.get_mut("A").and_then(Json::as_array_mut) {
                items.retain(|item| item["Id"].as_str().is_some_and(|id| known.contains(&id)));
            }
            out.push(json!({ "call": name, "args": render_typed(&args), "returns": rendered }));
        }
        Err(e) => {
            out.push(json!({ "call": name, "args": render_typed(&args), "error": e.to_string() }));
        }
    }
}

fn preference(user: &str, category: &str, name: &str, value: &str) -> Preference {
    Preference {
        user_id: user.to_owned(),
        category: category.to_owned(),
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

/// Every page is one page: `PerPage` far above what any stack holds, so a concurrent insert
/// elsewhere cannot move a known user across a page boundary.
fn everyone(active: bool, inactive: bool) -> Option<Box<UserGetOptions>> {
    Some(Box::new(UserGetOptions {
        per_page: 100_000,
        active,
        inactive,
        ..UserGetOptions::default()
    }))
}

async fn get_user(api: &Client, id: &str) -> Option<User> {
    let mut scratch = Vec::new();
    let found: Option<Z_GetUserReturns> = call(
        api,
        &mut scratch,
        "GetUser",
        Z_GetUserArgs { a: id.to_owned() },
    )
    .await;
    found.and_then(|f| f.a).map(|u| *u)
}

/// The reads of users that exist before any write, and the refusals that write nothing.
async fn user_reads(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let _: Option<Z_GetUsersByIdsReturns> = call(
        api,
        out,
        "GetUsersByIds",
        Z_GetUsersByIdsArgs {
            a: vec![
                input.own.clone(),
                input.reader.clone(),
                MISSING.to_owned(),
                input.other.clone(),
            ],
        },
    )
    .await;
    let _: Option<Z_GetUsersByIdsReturns> =
        call(api, out, "GetUsersByIds", Z_GetUsersByIdsArgs { a: vec![] }).await;

    let known = [
        input.own.as_str(),
        input.other.as_str(),
        input.reader.as_str(),
    ];
    call_filtered::<_, Z_GetUsersReturns>(
        api,
        out,
        "GetUsers",
        Z_GetUsersArgs {
            a: everyone(false, false),
        },
        &known,
    )
    .await;
    // A zero page is `LIMIT 0`. (Not nil options: Go dereferences them and the server dies.)
    let _: Option<Z_GetUsersReturns> = call(
        api,
        out,
        "GetUsers",
        Z_GetUsersArgs {
            a: Some(Box::default()),
        },
    )
    .await;

    for (team, page, per_page) in [
        (input.side_team.as_str(), 0, 100),
        (input.side_team.as_str(), 1, 2),
        (MISSING, 0, 10),
    ] {
        let _: Option<Z_GetUsersInTeamReturns> = call(
            api,
            out,
            "GetUsersInTeam",
            Z_GetUsersInTeamArgs {
                a: team.to_owned(),
                b: page,
                c: per_page,
            },
        )
        .await;
    }
    for (sort, page, per_page) in [("username", 0, 100), ("username", 1, 1), ("bogus", 0, 10)] {
        let _: Option<Z_GetUsersInChannelReturns> = call(
            api,
            out,
            "GetUsersInChannel",
            Z_GetUsersInChannelArgs {
                a: input.own_channel.clone(),
                b: sort.to_owned(),
                c: page,
                d: per_page,
            },
        )
        .await;
    }

    let own = get_user(api, &input.own).await.unwrap_or_default();
    for search in [
        // By username, in the side team.
        UserSearch {
            term: own.username.clone(),
            team_id: input.side_team.clone(),
            limit: 10,
            ..UserSearch::default()
        },
        // By the e-mail's domain, which a plugin's search does not look at.
        UserSearch {
            term: "mmrs.invalid".into(),
            team_id: input.side_team.clone(),
            limit: 10,
            ..UserSearch::default()
        },
        // In the channel, by the shared username prefix; a role that picks no arm.
        UserSearch {
            term: "mmrsplainusr".into(),
            in_channel_id: input.own_channel.clone(),
            role: "system_admin".into(),
            limit: 10,
            ..UserSearch::default()
        },
        // A zero limit is `LIMIT 0`: no default is applied.
        UserSearch {
            term: own.username.clone(),
            ..UserSearch::default()
        },
    ] {
        let _: Option<Z_SearchUsersReturns> = call(
            api,
            out,
            "SearchUsers",
            Z_SearchUsersArgs {
                a: Some(Box::new(search)),
            },
        )
        .await;
    }

    for id in [input.own.as_str(), MISSING] {
        let _: Option<Z_GetProfileImageReturns> = call(
            api,
            out,
            "GetProfileImage",
            Z_GetProfileImageArgs { a: id.to_owned() },
        )
        .await;
    }
}

/// A user made, refused, updated, re-roled, deactivated, reactivated and deleted; the own user's
/// profile and custom status. Answers the made user's id.
async fn user_writes(api: &Client, input: &Inputs, out: &mut Vec<Json>) -> String {
    let side = &input.side;
    let username = format!("mmrsplainusrnew{side}");
    let fresh = User {
        username: username.clone(),
        email: format!("{username}@mmrs.invalid"),
        password: PASSWORD.into(),
        nickname: "made by a plugin".into(),
        roles: "system_admin system_user".into(),
        ..User::default()
    };
    let created: Option<Z_CreateUserReturns> = call(
        api,
        out,
        "CreateUser",
        Z_CreateUserArgs {
            a: Some(Box::new(fresh.clone())),
        },
    )
    .await;
    let created_id = created.and_then(|c| c.a).map(|u| u.id).unwrap_or_default();
    // The same username again, and the zero user. (Not a nil user: Go dereferences it.)
    for user in [Some(Box::new(fresh)), Some(Box::default())] {
        let _: Option<Z_CreateUserReturns> =
            call(api, out, "CreateUser", Z_CreateUserArgs { a: user }).await;
    }

    // The own user, edited whole: a nickname, a position and a first name.
    let mut own = get_user(api, &input.own).await.unwrap_or_default();
    own.nickname = format!("plugnick{side}");
    own.position = "plugin tester".into();
    own.first_name = format!("Plugfirst{side}");
    let _: Option<Z_UpdateUserReturns> = call(
        api,
        out,
        "UpdateUser",
        Z_UpdateUserArgs {
            a: Some(Box::new(own.clone())),
        },
    )
    .await;
    own.id = MISSING.into();
    let _: Option<Z_UpdateUserReturns> = call(
        api,
        out,
        "UpdateUser",
        Z_UpdateUserArgs {
            a: Some(Box::new(own)),
        },
    )
    .await;
    // Found by nickname; not by first name, which a plugin's search does not look at.
    for term in [format!("plugnick{side}"), format!("plugfirst{side}")] {
        let _: Option<Z_SearchUsersReturns> = call(
            api,
            out,
            "SearchUsers",
            Z_SearchUsersArgs {
                a: Some(Box::new(UserSearch {
                    term,
                    limit: 10,
                    ..UserSearch::default()
                })),
            },
        )
        .await;
    }

    for (user, roles) in [
        (input.own.as_str(), "system_user"),
        (created_id.as_str(), "system_user nosuchpluginrole"),
        (MISSING, "system_user"),
    ] {
        let _: Option<Z_UpdateUserRolesReturns> = call(
            api,
            out,
            "UpdateUserRoles",
            Z_UpdateUserRolesArgs {
                a: user.to_owned(),
                b: roles.to_owned(),
            },
        )
        .await;
    }

    // Off and on again (it is deleted at the end); and a user that is nobody.
    for (user, active) in [
        (created_id.as_str(), false),
        (created_id.as_str(), true),
        (MISSING, true),
    ] {
        let _: Option<Z_UpdateUserActiveReturns> = call(
            api,
            out,
            "UpdateUserActive",
            Z_UpdateUserActiveArgs {
                a: user.to_owned(),
                b: active,
            },
        )
        .await;
    }
    // A custom status with a zone the plugin chose, one with an emoji nobody has, a nil one;
    // then it is cleared, and a user that is nobody.
    let expires = gobwire::GoTime::from_unix(4_102_424_999, 0, gobwire::Zone::Offset(19_800));
    for status in [
        Some(Box::new(CustomStatus {
            emoji: "smile".into(),
            text: format!("plugged in {side}"),
            duration: "date_and_time".into(),
            expires_at: expires,
        })),
        Some(Box::new(CustomStatus {
            emoji: "nosuchpluginemoji".into(),
            text: "nope".into(),
            ..CustomStatus::default()
        })),
        None,
    ] {
        let _: Option<Z_UpdateUserCustomStatusReturns> = call(
            api,
            out,
            "UpdateUserCustomStatus",
            Z_UpdateUserCustomStatusArgs {
                a: input.own.clone(),
                b: status,
            },
        )
        .await;
    }
    let _: Option<Z_GetUserReturns> = call(
        api,
        out,
        "GetUser",
        Z_GetUserArgs {
            a: input.own.clone(),
        },
    )
    .await;
    for user in [input.own.as_str(), MISSING] {
        let _: Option<Z_RemoveUserCustomStatusReturns> = call(
            api,
            out,
            "RemoveUserCustomStatus",
            Z_RemoveUserCustomStatusArgs { a: user.to_owned() },
        )
        .await;
    }
    created_id
}

/// Statuses: read, set by hand four ways and refused a fifth, a timed DND, and the channel
/// listing in status order once the two users' ranks disagree with their usernames'.
async fn statuses(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for user in [input.own.as_str(), input.other.as_str(), MISSING] {
        let _: Option<Z_GetUserStatusReturns> = call(
            api,
            out,
            "GetUserStatus",
            Z_GetUserStatusArgs { a: user.to_owned() },
        )
        .await;
    }
    for status in ["away", "offline", "online", "dnd", "ooo", "bogus"] {
        let _: Option<Z_UpdateUserStatusReturns> = call(
            api,
            out,
            "UpdateUserStatus",
            Z_UpdateUserStatusArgs {
                a: input.own.clone(),
                b: status.to_owned(),
            },
        )
        .await;
    }
    // From dnd to online, so the timed DND records `online` as the status to return to.
    let _: Option<Z_UpdateUserStatusReturns> = call(
        api,
        out,
        "UpdateUserStatus",
        Z_UpdateUserStatusArgs {
            a: input.own.clone(),
            b: "online".into(),
        },
    )
    .await;
    let _: Option<Z_SetUserStatusTimedDNDReturns> = call(
        api,
        out,
        "SetUserStatusTimedDND",
        Z_SetUserStatusTimedDNDArgs {
            a: input.own.clone(),
            b: 4_102_444_830,
        },
    )
    .await;
    // `own` online and `other` dnd: the status rank puts `own` first, where username order
    // (`usroth` < `usrown`) puts `other` first — so the two orders cannot coincide.
    for (user, status) in [(&input.own, "online"), (&input.other, "dnd")] {
        let _: Option<Z_UpdateUserStatusReturns> = call(
            api,
            out,
            "UpdateUserStatus",
            Z_UpdateUserStatusArgs {
                a: user.clone(),
                b: status.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_GetUserStatusesByIdsReturns> = call(
        api,
        out,
        "GetUserStatusesByIds",
        Z_GetUserStatusesByIdsArgs {
            a: vec![input.own.clone(), MISSING.to_owned(), input.other.clone()],
        },
    )
    .await;
    // `own` (online) before `other` (dnd), against username order; anyone else in the channel is
    // filtered out, since another user's status is shared-stack state.
    let known = [input.own.as_str(), input.other.as_str()];
    call_filtered::<_, Z_GetUsersInChannelReturns>(
        api,
        out,
        "GetUsersInChannel",
        Z_GetUsersInChannelArgs {
            a: input.own_channel.clone(),
            b: "status".into(),
            c: 0,
            d: 100,
        },
        &known,
    )
    .await;
}

/// `GetPreferenceForUser` of the one key the script writes and deletes.
async fn read_preference(api: &Client, own: &str, out: &mut Vec<Json>) {
    let _: Option<Z_GetPreferenceForUserReturns> = call(
        api,
        out,
        "GetPreferenceForUser",
        Z_GetPreferenceForUserArgs {
            a: own.to_owned(),
            b: "pluginusers".into(),
            c: "key".into(),
        },
    )
    .await;
}

/// The four preference methods: a miss, a save that fires the hook, a hit, two refusals of
/// another user's rows, and a delete that fires nothing.
async fn preferences(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let side = &input.side;
    read_preference(api, &input.own, out).await;
    let _: Option<Z_UpdatePreferencesForUserReturns> = call(
        api,
        out,
        "UpdatePreferencesForUser",
        Z_UpdatePreferencesForUserArgs {
            a: input.own.clone(),
            b: vec![
                preference(&input.own, "pluginusers", "key", &format!("value {side}")),
                preference(&input.own, "display_settings", "use_military_time", "true"),
            ],
        },
    )
    .await;
    read_preference(api, &input.own, out).await;
    let _: Option<Z_GetPreferencesForUserReturns> = call(
        api,
        out,
        "GetPreferencesForUser",
        Z_GetPreferencesForUserArgs {
            a: input.own.clone(),
        },
    )
    .await;
    let foreign = vec![
        preference(&input.own, "pluginusers", "mine", "v"),
        preference(&input.other, "pluginusers", "theirs", "v"),
    ];
    let _: Option<Z_UpdatePreferencesForUserReturns> = call(
        api,
        out,
        "UpdatePreferencesForUser",
        Z_UpdatePreferencesForUserArgs {
            a: input.own.clone(),
            b: foreign.clone(),
        },
    )
    .await;
    let _: Option<Z_DeletePreferencesForUserReturns> = call(
        api,
        out,
        "DeletePreferencesForUser",
        Z_DeletePreferencesForUserArgs {
            a: input.own.clone(),
            b: foreign,
        },
    )
    .await;
    let _: Option<Z_DeletePreferencesForUserReturns> = call(
        api,
        out,
        "DeletePreferencesForUser",
        Z_DeletePreferencesForUserArgs {
            a: input.own.clone(),
            b: vec![preference(&input.own, "pluginusers", "key", "")],
        },
    )
    .await;
    read_preference(api, &input.own, out).await;
}

/// The team reads before any team write, then a team made, joined by the own user and the made
/// user, re-roled, left by the made user, renamed, searched for and archived.
async fn teams(api: &Client, input: &Inputs, created_id: &str, out: &mut Vec<Json>) {
    let own = input.own.as_str();
    let side = &input.side;
    let _: Option<Z_GetTeamsForUserReturns> = call(
        api,
        out,
        "GetTeamsForUser",
        Z_GetTeamsForUserArgs { a: own.to_owned() },
    )
    .await;
    let _: Option<Z_GetTeamsUnreadForUserReturns> = call(
        api,
        out,
        "GetTeamsUnreadForUser",
        Z_GetTeamsUnreadForUserArgs { a: own.to_owned() },
    )
    .await;
    for team in [input.side_team.as_str(), MISSING] {
        let _: Option<Z_GetTeamStatsReturns> = call(
            api,
            out,
            "GetTeamStats",
            Z_GetTeamStatsArgs { a: team.to_owned() },
        )
        .await;
    }
    for page in [0, 1] {
        let _: Option<Z_GetTeamMembersReturns> = call(
            api,
            out,
            "GetTeamMembers",
            Z_GetTeamMembersArgs {
                a: input.side_team.clone(),
                b: page,
                c: 100,
            },
        )
        .await;
    }

    let name = format!("mmrs-parity-plugusers-{side}");
    let team = Team {
        name: name.clone(),
        display_name: "Plugin Users".into(),
        r#type: "O".into(),
        email: "plugin-team@mmrs.invalid".into(),
        ..Team::default()
    };
    let created: Option<Z_CreateTeamReturns> = call(
        api,
        out,
        "CreateTeam",
        Z_CreateTeamArgs {
            a: Some(Box::new(team.clone())),
        },
    )
    .await;
    let created = created.and_then(|c| c.a).map(|t| *t).unwrap_or_default();
    // The same name again, and the zero team. (Not a nil team: Go dereferences it.)
    for team in [Some(Box::new(team)), Some(Box::default())] {
        let _: Option<Z_CreateTeamReturns> =
            call(api, out, "CreateTeam", Z_CreateTeamArgs { a: team }).await;
    }

    // The joiners are the own user, whose socket is watched, and the made user. What the own
    // user's socket hears of its own default-channel joins is decided by when its membership
    // cache was loaded; Go's answer for off-topic's `user_added` is a coin toss (D-1032).
    for (team, user) in [(created.id.as_str(), own), (MISSING, own)] {
        let _: Option<Z_CreateTeamMemberReturns> = call(
            api,
            out,
            "CreateTeamMember",
            Z_CreateTeamMemberArgs {
                a: team.to_owned(),
                b: user.to_owned(),
            },
        )
        .await;
    }
    for users in [vec![created_id.to_owned()], vec![MISSING.to_owned()]] {
        let _: Option<Z_CreateTeamMembersReturns> = call(
            api,
            out,
            "CreateTeamMembers",
            Z_CreateTeamMembersArgs {
                a: created.id.clone(),
                b: users,
                c: own.to_owned(),
            },
        )
        .await;
    }
    for (user, roles) in [
        (created_id, "team_user team_admin"),
        (created_id, "team_user nosuchpluginrole"),
        (MISSING, "team_user"),
    ] {
        let _: Option<Z_UpdateTeamMemberRolesReturns> = call(
            api,
            out,
            "UpdateTeamMemberRoles",
            Z_UpdateTeamMemberRolesArgs {
                a: created.id.clone(),
                b: user.to_owned(),
                c: roles.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_GetTeamMembersReturns> = call(
        api,
        out,
        "GetTeamMembers",
        Z_GetTeamMembersArgs {
            a: created.id.clone(),
            b: 0,
            c: 100,
        },
    )
    .await;
    let _: Option<Z_GetTeamsForUserReturns> = call(
        api,
        out,
        "GetTeamsForUser",
        Z_GetTeamsForUserArgs { a: own.to_owned() },
    )
    .await;
    for user in [created_id, MISSING] {
        let _: Option<Z_DeleteTeamMemberReturns> = call(
            api,
            out,
            "DeleteTeamMember",
            Z_DeleteTeamMemberArgs {
                a: created.id.clone(),
                b: user.to_owned(),
                c: own.to_owned(),
            },
        )
        .await;
    }
    // The left team's row is still there, with its `DeleteAt`.
    for (user, page) in [(created_id, 0), (own, 0), (own, 1)] {
        let _: Option<Z_GetTeamMembersForUserReturns> = call(
            api,
            out,
            "GetTeamMembersForUser",
            Z_GetTeamMembersForUserArgs {
                a: user.to_owned(),
                b: page,
                c: 100,
            },
        )
        .await;
    }
    let _: Option<Z_GetTeamsUnreadForUserReturns> = call(
        api,
        out,
        "GetTeamsUnreadForUser",
        Z_GetTeamsUnreadForUserArgs { a: own.to_owned() },
    )
    .await;

    let mut renamed = created.clone();
    renamed.display_name = "Plugin Users Renamed".into();
    renamed.description = format!("edited by the {side} plugin");
    renamed.r#type = "I".into();
    let _: Option<Z_UpdateTeamReturns> = call(
        api,
        out,
        "UpdateTeam",
        Z_UpdateTeamArgs {
            a: Some(Box::new(renamed.clone())),
        },
    )
    .await;
    renamed.id = MISSING.into();
    let _: Option<Z_UpdateTeamReturns> = call(
        api,
        out,
        "UpdateTeam",
        Z_UpdateTeamArgs {
            a: Some(Box::new(renamed)),
        },
    )
    .await;
    for term in [format!("plugusers-{side}"), "nosuchpluginteam".into()] {
        let _: Option<Z_SearchTeamsReturns> =
            call(api, out, "SearchTeams", Z_SearchTeamsArgs { a: term }).await;
    }
    let known = [
        input.side_team.as_str(),
        input.shared_team.as_str(),
        created.id.as_str(),
    ];
    call_filtered::<_, Z_GetTeamsReturns>(api, out, "GetTeams", Z_GetTeamsArgs {}, &known).await;
    for team in [created.id.as_str(), MISSING] {
        let _: Option<Z_DeleteTeamReturns> = call(
            api,
            out,
            "DeleteTeam",
            Z_DeleteTeamArgs { a: team.to_owned() },
        )
        .await;
    }
    call_filtered::<_, Z_GetTeamsReturns>(api, out, "GetTeams", Z_GetTeamsArgs {}, &known).await;
    let _: Option<Z_GetTeamsForUserReturns> = call(
        api,
        out,
        "GetTeamsForUser",
        Z_GetTeamsForUserArgs { a: own.to_owned() },
    )
    .await;
}

/// The made user deleted — a deactivation — and found by the active and inactive listings and
/// the inactive-allowing search accordingly.
async fn retire(api: &Client, input: &Inputs, created_id: &str, out: &mut Vec<Json>) {
    let username = format!("mmrsplainusrnew{}", input.side);
    for user in [created_id, MISSING] {
        let _: Option<Z_DeleteUserReturns> = call(
            api,
            out,
            "DeleteUser",
            Z_DeleteUserArgs { a: user.to_owned() },
        )
        .await;
    }
    let known = [
        input.own.as_str(),
        input.other.as_str(),
        input.reader.as_str(),
        created_id,
    ];
    for (active, inactive) in [(true, false), (false, true)] {
        call_filtered::<_, Z_GetUsersReturns>(
            api,
            out,
            "GetUsers",
            Z_GetUsersArgs {
                a: everyone(active, inactive),
            },
            &known,
        )
        .await;
    }
    for allow_inactive in [false, true] {
        let _: Option<Z_SearchUsersReturns> = call(
            api,
            out,
            "SearchUsers",
            Z_SearchUsersArgs {
                a: Some(Box::new(UserSearch {
                    term: username.clone(),
                    allow_inactive,
                    limit: 10,
                    ..UserSearch::default()
                })),
            },
        )
        .await;
    }
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    user_reads(api, input, &mut out).await;
    let created = user_writes(api, input, &mut out).await;
    statuses(api, input, &mut out).await;
    preferences(api, input, &mut out).await;
    teams(api, input, &created, &mut out).await;
    retire(api, input, &created, &mut out).await;
    out
}
