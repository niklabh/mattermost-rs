//! The hook recorder's core script: the plugin API methods a plugin calls after activation —
//! users, teams, channels, posts, permissions, bots and websocket events — each written down with
//! what the host answered, for `parity::plugin_hooks`' core tranche (docs/PLUGIN_PLAN.md,
//! Phase 6).
//!
//! It runs when a post's message is [`CORE_SCRIPT`], from inside `MessageWillBePosted`, like the
//! KV script. Unlike that one it **writes posts**, so the recorder's own message hooks fire for
//! them while the outer hook is still waiting — which is exactly what a real plugin's
//! `CreatePost` from inside a hook does.
//!
//! # What the suite hands it
//!
//! Through the environment the host passes down: a **reader** and a **read channel** that both
//! sides share and only read; the admin, whose permissions are asked about; and this side's
//! **own** user and **other** user, and a **side** tag (`sidego` or `siders`) that every name the
//! script makes carries, so the suite can scrub one side's names into the other's. The own
//! channel is the channel the trigger post was made in, and the session is the trigger's.
//!
//! Every write lands on something this side owns, so the second side never finds the first
//! side's rows; the two bots it makes are permanently deleted and the bot KV key cleared at the
//! end, so the second side starts where the first did.

use std::collections::HashMap;

use go_netrpc::Client;
use gobwire::{Decode, Encode, Interface};
use mm_plugin::wire::model::{
    Bot, BotGetOptions, BotPatch, Channel, Permission, Post, Reaction, WebsocketBroadcast,
};
use mm_plugin::wire::plugin::*;
use serde_json::{Value as Json, json};

use crate::render::render_typed;

/// The message that runs the script.
pub const CORE_SCRIPT: &str = "!core-script";

/// A well-formed id that names nothing.
pub const MISSING: &str = "coremissingcoremissingcore";

/// What the suite put in the environment, and what the trigger carried.
pub struct Inputs {
    pub reader: String,
    pub read_channel: String,
    pub admin: String,
    pub own: String,
    pub other: String,
    pub side: String,
    pub own_channel: String,
    pub session: String,
}

impl Inputs {
    /// The environment's half, with the trigger's channel and session.
    pub fn from_env(own_channel: &str, session: &str) -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            reader: var("HOOK_RECORDER_CORE_READER"),
            read_channel: var("HOOK_RECORDER_CORE_READ_CHANNEL"),
            admin: var("HOOK_RECORDER_CORE_ADMIN"),
            own: var("HOOK_RECORDER_CORE_OWN"),
            other: var("HOOK_RECORDER_CORE_OTHER"),
            side: var("HOOK_RECORDER_CORE_SIDE"),
            own_channel: own_channel.to_owned(),
            session: session.to_owned(),
        }
    }
}

/// One call, written down with its arguments and what came back — or the transport error, such
/// as `API <Name> called but not implemented.` — and the typed answer for the script to use.
async fn call<A, R>(api: &Client, out: &mut Vec<Json>, name: &str, args: A) -> Option<R>
where
    A: Encode,
    R: Decode + Default + Encode + Send + 'static,
{
    match api.call::<A, R>(&format!("Plugin.{name}"), &args).await {
        Ok(returns) => {
            out.push(json!({
                "call": name,
                "args": render_typed(&args),
                "returns": render_typed(&returns),
            }));
            Some(returns)
        }
        Err(e) => {
            out.push(json!({ "call": name, "args": render_typed(&args), "error": e.to_string() }));
            None
        }
    }
}

fn permission(id: &str) -> Option<Box<Permission>> {
    Some(Box::new(Permission {
        id: id.to_owned(),
        ..Permission::default()
    }))
}

fn post(channel: &str, user: &str, message: &str) -> Option<Box<Post>> {
    Some(Box::new(Post {
        channel_id: channel.to_owned(),
        user_id: user.to_owned(),
        message: message.to_owned(),
        ..Post::default()
    }))
}

fn reaction(user: &str, post: &str, emoji: &str) -> Option<Box<Reaction>> {
    Some(Box::new(Reaction {
        user_id: user.to_owned(),
        post_id: post.to_owned(),
        emoji_name: emoji.to_owned(),
        ..Reaction::default()
    }))
}

fn bot(username: &str, display_name: &str, owner: &str) -> Option<Box<Bot>> {
    Some(Box::new(Bot {
        username: username.to_owned(),
        display_name: display_name.to_owned(),
        description: "made by the core script".to_owned(),
        owner_id: owner.to_owned(),
        ..Bot::default()
    }))
}

/// Every JSON kind a payload can carry, as a Go plugin's `map[string]any` would hold them.
fn payload() -> HashMap<String, Option<Interface>> {
    HashMap::from([
        ("text".to_owned(), Some(Interface::string("hello <&>"))),
        ("count".to_owned(), Some(Interface::int(3))),
        ("ratio".to_owned(), Some(Interface::float64(1.5))),
        ("whole".to_owned(), Some(Interface::float64(2.0))),
        ("flag".to_owned(), Some(Interface::bool(true))),
        (
            "nested".to_owned(),
            mm_plugin::rpc::json_to_interface(&json!({ "list": [1, "two", null], "k": "v" })),
        ),
        ("nothing".to_owned(), None),
    ])
}

async fn publish(
    api: &Client,
    out: &mut Vec<Json>,
    event: &str,
    payload: HashMap<String, Option<Interface>>,
    broadcast: WebsocketBroadcast,
) {
    let _: Option<Z_PublishWebSocketEventReturns> = call(
        api,
        out,
        "PublishWebSocketEvent",
        Z_PublishWebSocketEventArgs {
            a: event.to_owned(),
            b: payload,
            c: Some(Box::new(broadcast)),
        },
    )
    .await;
}

/// The reads of what both sides share, the not-found errors, and the permission checks.
async fn reads(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let reader: Option<Z_GetUserReturns> = call(
        api,
        out,
        "GetUser",
        Z_GetUserArgs {
            a: input.reader.clone(),
        },
    )
    .await;
    let (username, email) = reader
        .and_then(|r| r.a)
        .map(|u| (u.username, u.email))
        .unwrap_or_default();
    let _: Option<Z_GetUserReturns> =
        call(api, out, "GetUser", Z_GetUserArgs { a: MISSING.into() }).await;
    let _: Option<Z_GetUserByUsernameReturns> = call(
        api,
        out,
        "GetUserByUsername",
        Z_GetUserByUsernameArgs {
            a: username.clone(),
        },
    )
    .await;
    let _: Option<Z_GetUserByUsernameReturns> = call(
        api,
        out,
        "GetUserByUsername",
        Z_GetUserByUsernameArgs {
            a: "nosuchcoreuser".into(),
        },
    )
    .await;
    let _: Option<Z_GetUserByEmailReturns> = call(
        api,
        out,
        "GetUserByEmail",
        Z_GetUserByEmailArgs { a: email },
    )
    .await;
    let _: Option<Z_GetUserByEmailReturns> = call(
        api,
        out,
        "GetUserByEmail",
        Z_GetUserByEmailArgs {
            a: "nobody-core@mmrs.invalid".into(),
        },
    )
    .await;
    let _: Option<Z_GetUsersByUsernamesReturns> = call(
        api,
        out,
        "GetUsersByUsernames",
        Z_GetUsersByUsernamesArgs {
            a: vec![username.clone(), "nosuchcoreuser".into()],
        },
    )
    .await;

    let channel: Option<Z_GetChannelReturns> = call(
        api,
        out,
        "GetChannel",
        Z_GetChannelArgs {
            a: input.read_channel.clone(),
        },
    )
    .await;
    let (team, channel_name) = channel
        .and_then(|c| c.a)
        .map(|c| (c.team_id, c.name))
        .unwrap_or_default();
    let _: Option<Z_GetChannelReturns> = call(
        api,
        out,
        "GetChannel",
        Z_GetChannelArgs { a: MISSING.into() },
    )
    .await;
    let found: Option<Z_GetTeamReturns> =
        call(api, out, "GetTeam", Z_GetTeamArgs { a: team.clone() }).await;
    let team_name = found.and_then(|t| t.a).map(|t| t.name).unwrap_or_default();
    let _: Option<Z_GetTeamReturns> =
        call(api, out, "GetTeam", Z_GetTeamArgs { a: MISSING.into() }).await;
    let _: Option<Z_GetTeamByNameReturns> = call(
        api,
        out,
        "GetTeamByName",
        Z_GetTeamByNameArgs {
            a: team_name.clone(),
        },
    )
    .await;
    let _: Option<Z_GetTeamByNameReturns> = call(
        api,
        out,
        "GetTeamByName",
        Z_GetTeamByNameArgs {
            a: "nosuchcoreteam".into(),
        },
    )
    .await;
    for user in [input.reader.as_str(), MISSING] {
        let _: Option<Z_GetTeamMemberReturns> = call(
            api,
            out,
            "GetTeamMember",
            Z_GetTeamMemberArgs {
                a: team.clone(),
                b: user.to_owned(),
            },
        )
        .await;
    }
    for (name, deleted) in [(channel_name.as_str(), false), ("nosuchcorechannel", false)] {
        let _: Option<Z_GetChannelByNameReturns> = call(
            api,
            out,
            "GetChannelByName",
            Z_GetChannelByNameArgs {
                a: team.clone(),
                b: name.to_owned(),
                c: deleted,
            },
        )
        .await;
    }
    for team_name in [team_name.as_str(), "nosuchcoreteam"] {
        let _: Option<Z_GetChannelByNameForTeamNameReturns> = call(
            api,
            out,
            "GetChannelByNameForTeamName",
            Z_GetChannelByNameForTeamNameArgs {
                a: team_name.to_owned(),
                b: channel_name.clone(),
                c: false,
            },
        )
        .await;
    }
    for user in [input.reader.as_str(), MISSING] {
        let _: Option<Z_GetChannelMemberReturns> = call(
            api,
            out,
            "GetChannelMember",
            Z_GetChannelMemberArgs {
                a: input.read_channel.clone(),
                b: user.to_owned(),
            },
        )
        .await;
    }
    for session in [input.session.as_str(), MISSING] {
        let _: Option<Z_GetSessionReturns> = call(
            api,
            out,
            "GetSession",
            Z_GetSessionArgs {
                a: session.to_owned(),
            },
        )
        .await;
    }

    // Permissions: an admin and a plain user system-wide, a member's team rights, a channel
    // member's rights, and a channel that does not exist.
    for (user, perm) in [
        (input.admin.as_str(), "manage_system"),
        (input.reader.as_str(), "manage_system"),
        (input.reader.as_str(), "create_team"),
    ] {
        let _: Option<Z_HasPermissionToReturns> = call(
            api,
            out,
            "HasPermissionTo",
            Z_HasPermissionToArgs {
                a: user.to_owned(),
                b: permission(perm),
            },
        )
        .await;
    }
    for (user, perm) in [
        (input.reader.as_str(), "view_team"),
        (input.reader.as_str(), "manage_team"),
        (MISSING, "view_team"),
        (input.admin.as_str(), "manage_team"),
    ] {
        let _: Option<Z_HasPermissionToTeamReturns> = call(
            api,
            out,
            "HasPermissionToTeam",
            Z_HasPermissionToTeamArgs {
                a: user.to_owned(),
                b: team.clone(),
                c: permission(perm),
            },
        )
        .await;
    }
    for (user, channel, perm) in [
        (
            input.reader.as_str(),
            input.read_channel.as_str(),
            "read_channel",
        ),
        (
            input.reader.as_str(),
            input.read_channel.as_str(),
            "manage_channel_roles",
        ),
        (input.reader.as_str(), MISSING, "read_channel"),
        (
            input.own.as_str(),
            input.read_channel.as_str(),
            "read_channel",
        ),
    ] {
        let _: Option<Z_HasPermissionToChannelReturns> = call(
            api,
            out,
            "HasPermissionToChannel",
            Z_HasPermissionToChannelArgs {
                a: user.to_owned(),
                b: channel.to_owned(),
                c: permission(perm),
            },
        )
        .await;
    }
}

/// The bot methods, and `EnsureBotUser` as the SDK's `EnsureBot` calls it. Answers the two bot
/// ids it made.
async fn bots(api: &Client, input: &Inputs, out: &mut Vec<Json>) -> (String, String) {
    let side = &input.side;
    let created: Option<Z_CreateBotReturns> = call(
        api,
        out,
        "CreateBot",
        Z_CreateBotArgs {
            a: bot(&format!("corebot{side}"), "Core Bot", ""),
        },
    )
    .await;
    let bot_id = created
        .and_then(|c| c.a)
        .map(|b| b.user_id)
        .unwrap_or_default();
    // A bot owned by a bot, and a username the model refuses.
    let _: Option<Z_CreateBotReturns> = call(
        api,
        out,
        "CreateBot",
        Z_CreateBotArgs {
            a: bot(&format!("corebotb{side}"), "Owned by a bot", &bot_id),
        },
    )
    .await;
    let _: Option<Z_CreateBotReturns> = call(
        api,
        out,
        "CreateBot",
        Z_CreateBotArgs {
            a: bot("Not A Valid Name", "Invalid", ""),
        },
    )
    .await;
    for id in [bot_id.as_str(), MISSING] {
        let _: Option<Z_GetBotReturns> = call(
            api,
            out,
            "GetBot",
            Z_GetBotArgs {
                a: id.to_owned(),
                b: false,
            },
        )
        .await;
    }
    let _: Option<Z_GetBotsReturns> = call(
        api,
        out,
        "GetBots",
        Z_GetBotsArgs {
            a: Some(Box::new(BotGetOptions {
                owner_id: "mmrs.hookrecorder".into(),
                per_page: 20,
                ..BotGetOptions::default()
            })),
        },
    )
    .await;
    for id in [bot_id.as_str(), MISSING] {
        let _: Option<Z_PatchBotReturns> = call(
            api,
            out,
            "PatchBot",
            Z_PatchBotArgs {
                a: id.to_owned(),
                b: Some(Box::new(BotPatch {
                    display_name: Some("Patched Core Bot".into()),
                    ..BotPatch::default()
                })),
            },
        )
        .await;
    }
    // Disabled, invisible without deleted ones, visible with them, enabled again.
    let _: Option<Z_UpdateBotActiveReturns> = call(
        api,
        out,
        "UpdateBotActive",
        Z_UpdateBotActiveArgs {
            a: bot_id.clone(),
            b: false,
        },
    )
    .await;
    for deleted in [false, true] {
        let _: Option<Z_GetBotReturns> = call(
            api,
            out,
            "GetBot",
            Z_GetBotArgs {
                a: bot_id.clone(),
                b: deleted,
            },
        )
        .await;
    }
    let _: Option<Z_UpdateBotActiveReturns> = call(
        api,
        out,
        "UpdateBotActive",
        Z_UpdateBotActiveArgs {
            a: bot_id.clone(),
            b: true,
        },
    )
    .await;
    let _: Option<Z_UpdateBotActiveReturns> = call(
        api,
        out,
        "UpdateBotActive",
        Z_UpdateBotActiveArgs {
            a: MISSING.into(),
            b: true,
        },
    )
    .await;

    // `EnsureBot`: a human's username, no username, a new bot, the same bot again (patched from
    // the KV key), and a nil bot.
    let reader_name = {
        let mut scratch = Vec::new();
        let reader: Option<Z_GetUserReturns> = call(
            api,
            &mut scratch,
            "GetUser",
            Z_GetUserArgs {
                a: input.reader.clone(),
            },
        )
        .await;
        reader
            .and_then(|r| r.a)
            .map(|u| u.username)
            .unwrap_or_default()
    };
    for (username, display) in [(reader_name.as_str(), "Taken"), ("", "Nameless")] {
        let _: Option<Z_EnsureBotUserReturns> = call(
            api,
            out,
            "EnsureBotUser",
            Z_EnsureBotUserArgs {
                a: bot(username, display, ""),
            },
        )
        .await;
    }
    let ensured: Option<Z_EnsureBotUserReturns> = call(
        api,
        out,
        "EnsureBotUser",
        Z_EnsureBotUserArgs {
            a: bot(&format!("ensured{side}"), "Ensured", "someone-else"),
        },
    )
    .await;
    let ensured_id = ensured.map(|e| e.a).unwrap_or_default();
    let _: Option<Z_EnsureBotUserReturns> = call(
        api,
        out,
        "EnsureBotUser",
        Z_EnsureBotUserArgs {
            a: bot(&format!("ensured{side}"), "Ensured again", ""),
        },
    )
    .await;
    let _: Option<Z_GetBotReturns> = call(
        api,
        out,
        "GetBot",
        Z_GetBotArgs {
            a: ensured_id.clone(),
            b: false,
        },
    )
    .await;
    (bot_id, ensured_id)
}

/// The post writes and reads, the ephemeral three, the reactions.
async fn posts(api: &Client, input: &Inputs, bot_id: &str, out: &mut Vec<Json>) {
    let channel = input.own_channel.as_str();
    let root: Option<Z_CreatePostReturns> = call(
        api,
        out,
        "CreatePost",
        Z_CreatePostArgs {
            a: post(channel, &input.own, "core: by the user"),
        },
    )
    .await;
    let root = root.and_then(|r| r.a).map(|p| *p).unwrap_or_default();
    // A reply by the bot, which the thread read below finds.
    let mut reply = post(channel, bot_id, "core: a reply by the bot");
    if let Some(reply) = reply.as_mut() {
        reply.root_id.clone_from(&root.id);
    }
    let reply: Option<Z_CreatePostReturns> =
        call(api, out, "CreatePost", Z_CreatePostArgs { a: reply }).await;
    let reply_id = reply.and_then(|r| r.a).map(|p| p.id).unwrap_or_default();
    // A silent post by a human author, which only a plugin (or an integration) may make.
    let mut silent = post(channel, &input.own, "core: silently");
    if let Some(silent) = silent.as_mut() {
        silent
            .props
            .insert("silent_notification".into(), Some(Interface::bool(true)));
    }
    let _: Option<Z_CreatePostReturns> =
        call(api, out, "CreatePost", Z_CreatePostArgs { a: silent }).await;
    // A missing channel, a missing author, and the recorder's own hook refusing a post the
    // recorder made.
    for args in [
        post(MISSING, &input.own, "core: nowhere"),
        post(channel, MISSING, "core: nobody"),
        post(channel, &input.own, "!reject core refuses its own post"),
    ] {
        let _: Option<Z_CreatePostReturns> =
            call(api, out, "CreatePost", Z_CreatePostArgs { a: args }).await;
    }

    for id in [root.id.as_str(), MISSING] {
        let _: Option<Z_GetPostReturns> =
            call(api, out, "GetPost", Z_GetPostArgs { a: id.to_owned() }).await;
        let _: Option<Z_GetPostThreadReturns> = call(
            api,
            out,
            "GetPostThread",
            Z_GetPostThreadArgs { a: id.to_owned() },
        )
        .await;
    }

    let mut edited = root.clone();
    edited.message = "core: edited by the plugin".into();
    let _: Option<Z_UpdatePostReturns> = call(
        api,
        out,
        "UpdatePost",
        Z_UpdatePostArgs {
            a: Some(Box::new(edited)),
        },
    )
    .await;
    let mut missing = root.clone();
    missing.id = MISSING.into();
    let _: Option<Z_UpdatePostReturns> = call(
        api,
        out,
        "UpdatePost",
        Z_UpdatePostArgs {
            a: Some(Box::new(missing)),
        },
    )
    .await;

    for emoji in ["smile", "nosuchcoreemoji"] {
        let _: Option<Z_AddReactionReturns> = call(
            api,
            out,
            "AddReaction",
            Z_AddReactionArgs {
                a: reaction(&input.own, &root.id, emoji),
            },
        )
        .await;
    }
    let _: Option<Z_RemoveReactionReturns> = call(
        api,
        out,
        "RemoveReaction",
        Z_RemoveReactionArgs {
            a: reaction(&input.own, &root.id, "smile"),
        },
    )
    .await;

    // Ephemeral: sent to the own user as the bot, updated, deleted.
    let sent: Option<Z_SendEphemeralPostReturns> = call(
        api,
        out,
        "SendEphemeralPost",
        Z_SendEphemeralPostArgs {
            a: input.own.clone(),
            b: post(channel, bot_id, "core: only you can see this"),
        },
    )
    .await;
    let mut ephemeral = sent.and_then(|s| s.a).map(|p| *p).unwrap_or_default();
    let ephemeral_id = ephemeral.id.clone();
    ephemeral.message = "core: only you, edited".into();
    let _: Option<Z_UpdateEphemeralPostReturns> = call(
        api,
        out,
        "UpdateEphemeralPost",
        Z_UpdateEphemeralPostArgs {
            a: input.own.clone(),
            b: Some(Box::new(ephemeral)),
        },
    )
    .await;
    let _: Option<Z_DeleteEphemeralPostReturns> = call(
        api,
        out,
        "DeleteEphemeralPost",
        Z_DeleteEphemeralPostArgs {
            a: input.own.clone(),
            b: ephemeral_id,
        },
    )
    .await;

    for id in [reply_id.as_str(), MISSING] {
        let _: Option<Z_DeletePostReturns> = call(
            api,
            out,
            "DeletePost",
            Z_DeletePostArgs { a: id.to_owned() },
        )
        .await;
    }
}

/// The channel writes: a member added, a channel created, a DM and a GM got-or-created.
async fn channels(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for channel in [input.own_channel.as_str(), MISSING] {
        let _: Option<Z_AddChannelMemberReturns> = call(
            api,
            out,
            "AddChannelMember",
            Z_AddChannelMemberArgs {
                a: channel.to_owned(),
                b: input.other.clone(),
            },
        )
        .await;
    }
    let team = {
        let mut scratch = Vec::new();
        let found: Option<Z_GetChannelReturns> = call(
            api,
            &mut scratch,
            "GetChannel",
            Z_GetChannelArgs {
                a: input.own_channel.clone(),
            },
        )
        .await;
        found
            .and_then(|c| c.a)
            .map(|c| c.team_id)
            .unwrap_or_default()
    };
    let mut created = String::new();
    for (name, display) in [
        (format!("core{}", input.side), "Core Channel"),
        (String::new(), "No Name"),
    ] {
        let answer: Option<Z_CreateChannelReturns> = call(
            api,
            out,
            "CreateChannel",
            Z_CreateChannelArgs {
                a: Some(Box::new(Channel {
                    team_id: team.clone(),
                    name,
                    display_name: display.to_owned(),
                    r#type: "O".into(),
                    creator_id: input.own.clone(),
                    ..Channel::default()
                })),
            },
        )
        .await;
        if let Some(channel) = answer.and_then(|a| a.a) {
            created = channel.id;
        }
    }
    // `CreateChannel` from a plugin adds no member, not even the creator it names.
    let _: Option<Z_GetChannelMemberReturns> = call(
        api,
        out,
        "GetChannelMember",
        Z_GetChannelMemberArgs {
            a: created,
            b: input.own.clone(),
        },
    )
    .await;
    for other in [input.other.as_str(), MISSING] {
        let _: Option<Z_GetDirectChannelReturns> = call(
            api,
            out,
            "GetDirectChannel",
            Z_GetDirectChannelArgs {
                a: input.own.clone(),
                b: other.to_owned(),
            },
        )
        .await;
    }
    for members in [
        vec![input.own.clone(), input.other.clone(), input.reader.clone()],
        vec![input.own.clone(), input.other.clone()],
    ] {
        let _: Option<Z_GetGroupChannelReturns> = call(
            api,
            out,
            "GetGroupChannel",
            Z_GetGroupChannelArgs { a: members },
        )
        .await;
    }
}

/// Four events: to the own user, to the own channel, to the channel with the own user omitted
/// (so it must not arrive), and an empty payload.
async fn websocket(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    publish(
        api,
        out,
        "to_user",
        payload(),
        WebsocketBroadcast {
            user_id: input.own.clone(),
            ..WebsocketBroadcast::default()
        },
    )
    .await;
    publish(
        api,
        out,
        "to_channel",
        payload(),
        WebsocketBroadcast {
            channel_id: input.own_channel.clone(),
            ..WebsocketBroadcast::default()
        },
    )
    .await;
    publish(
        api,
        out,
        "omitted",
        payload(),
        WebsocketBroadcast {
            channel_id: input.own_channel.clone(),
            omit_users: HashMap::from([(input.own.clone(), true)]),
            ..WebsocketBroadcast::default()
        },
    )
    .await;
    publish(
        api,
        out,
        "empty",
        HashMap::new(),
        WebsocketBroadcast {
            user_id: input.own.clone(),
            ..WebsocketBroadcast::default()
        },
    )
    .await;
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    reads(api, input, &mut out).await;
    let (bot_id, ensured_id) = bots(api, input, &mut out).await;
    posts(api, input, &bot_id, &mut out).await;
    channels(api, input, &mut out).await;
    websocket(api, input, &mut out).await;

    // Clean up, so the other side starts where this one did.
    for id in [bot_id.as_str(), ensured_id.as_str(), MISSING] {
        let _: Option<Z_PermanentDeleteBotReturns> = call(
            api,
            &mut out,
            "PermanentDeleteBot",
            Z_PermanentDeleteBotArgs { a: id.to_owned() },
        )
        .await;
    }
    // Not `GetUser`: Go answers a permanently deleted bot's user from its user cache, which the
    // delete does not invalidate. The suite reads the rows instead.
    let _: Option<Z_GetBotReturns> =
        call(api, &mut out, "GetBot", Z_GetBotArgs { a: bot_id, b: true }).await;
    let _: Option<Z_KVDeleteReturns> = call(
        api,
        &mut out,
        "KVDelete",
        Z_KVDeleteArgs {
            a: "mmi_botid".into(),
        },
    )
    .await;
    out
}
