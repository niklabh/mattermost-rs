//! The hook recorder's server script: the plugin API's command, plugin, upload-session,
//! team-icon, profile-image, typing, toast, push, channel-restore, cluster and audit methods,
//! each written down with what the host answered, for `parity::plugin_hooks`' server tranche
//! (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`SERVER_SCRIPT`], from inside `MessageWillBePosted`, like
//! the channels script. Every hook its writes fire lands in the transcript beside its entry.
//!
//! # What the suite hands it
//!
//! Through the environment: this side's **own** and **other** users, an **outsider** (on the
//! side team's roster once and removed), the **side team** and the **side** tag. The own channel
//! is the trigger's.
//!
//! Everything it makes carries the side tag — the command's trigger, the channel's name, the
//! upload session's id — so the suite can scrub the tag and find the rows to remove.

use go_netrpc::Client;
use mm_plugin::wire::model::{
    AuditRecord, Channel, Command, PluginClusterEvent, PluginClusterEventSendOptions,
    PushNotification, SendToastMessageOptions, UploadSession,
};
use mm_plugin::wire::plugin::*;
use serde_json::Value as Json;

use crate::core::{MISSING, call};

/// The message that runs the script.
pub const SERVER_SCRIPT: &str = "!server-script";

/// A 1×1 PNG, which the icon and profile pipelines fill to 128×128.
pub const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1,
    1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// What the suite put in the environment, and the trigger's channel.
pub struct Inputs {
    pub own: String,
    pub other: String,
    pub outsider: String,
    pub side_team: String,
    pub side: String,
    pub own_channel: String,
}

impl Inputs {
    /// The environment's half, with the trigger's channel.
    pub fn from_env(own_channel: &str) -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            own: var("HOOK_RECORDER_SERVER_OWN"),
            other: var("HOOK_RECORDER_SERVER_OTHER"),
            outsider: var("HOOK_RECORDER_SERVER_OUTSIDER"),
            side_team: var("HOOK_RECORDER_SERVER_TEAM"),
            side: var("HOOK_RECORDER_SERVER_SIDE"),
            own_channel: own_channel.to_owned(),
        }
    }

    /// This side's upload session id: 26 characters, the side tag inside.
    pub fn upload_id(&self) -> String {
        format!("srvupload{}xxxxxxxx", self.side)
    }
}

/// Channels: a type-checked read, an add that names who added, and a restore.
async fn channels(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for (id, kind) in [
        (input.own_channel.as_str(), "O"),
        (input.own_channel.as_str(), "P"),
        (MISSING, "O"),
    ] {
        let _: Option<Z_GetChannelOfTypeReturns> = call(
            api,
            out,
            "GetChannelOfType",
            Z_GetChannelOfTypeArgs {
                a: id.to_owned(),
                b: kind.to_owned(),
            },
        )
        .await;
    }

    // A channel of the script's own, the other user added to it by the own user, archived and
    // restored twice.
    let side = &input.side;
    let made: Option<Z_CreateChannelReturns> = call(
        api,
        out,
        "CreateChannel",
        Z_CreateChannelArgs {
            a: Some(Box::new(Channel {
                team_id: input.side_team.clone(),
                r#type: "O".into(),
                name: format!("srvchan-{side}"),
                display_name: format!("Srv Chan {side}"),
                creator_id: input.own.clone(),
                ..Channel::default()
            })),
        },
    )
    .await;
    let made = made.and_then(|m| m.a).map(|c| c.id).unwrap_or_default();
    for (channel, user) in [
        (made.as_str(), input.own.as_str()),
        (made.as_str(), input.other.as_str()),
        (input.own_channel.as_str(), input.other.as_str()),
        (MISSING, input.other.as_str()),
        (made.as_str(), MISSING),
    ] {
        let _: Option<Z_AddUserToChannelReturns> = call(
            api,
            out,
            "AddUserToChannel",
            Z_AddUserToChannelArgs {
                a: channel.to_owned(),
                b: user.to_owned(),
                c: input.own.clone(),
            },
        )
        .await;
    }
    let _: Option<Z_DeleteChannelReturns> = call(
        api,
        out,
        "DeleteChannel",
        Z_DeleteChannelArgs { a: made.clone() },
    )
    .await;
    for id in [made.as_str(), made.as_str(), MISSING] {
        let _: Option<Z_RestoreChannelReturns> = call(
            api,
            out,
            "RestoreChannel",
            Z_RestoreChannelArgs { a: id.to_owned() },
        )
        .await;
    }
}

/// Teams: a graceful batch add, and the icon set, read, refused, removed and read again.
async fn teams(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let _: Option<Z_CreateTeamMembersGracefullyReturns> = call(
        api,
        out,
        "CreateTeamMembersGracefully",
        Z_CreateTeamMembersGracefullyArgs {
            a: input.side_team.clone(),
            b: vec![
                input.own.clone(),
                input.outsider.clone(),
                MISSING.to_owned(),
            ],
            c: input.own.clone(),
        },
    )
    .await;
    let _: Option<Z_CreateTeamMembersGracefullyReturns> = call(
        api,
        out,
        "CreateTeamMembersGracefully",
        Z_CreateTeamMembersGracefullyArgs {
            a: MISSING.to_owned(),
            b: vec![input.own.clone()],
            c: input.own.clone(),
        },
    )
    .await;

    let team = input.side_team.as_str();
    let get = |id: &str| Z_GetTeamIconArgs { a: id.to_owned() };
    let _: Option<Z_GetTeamIconReturns> = call(api, out, "GetTeamIcon", get(team)).await;
    for (id, data) in [
        (team, TINY_PNG.to_vec()),
        (team, b"not an image".to_vec()),
        (MISSING, TINY_PNG.to_vec()),
    ] {
        let _: Option<Z_SetTeamIconReturns> = call(
            api,
            out,
            "SetTeamIcon",
            Z_SetTeamIconArgs {
                a: id.to_owned(),
                b: data,
            },
        )
        .await;
    }
    let _: Option<Z_GetTeamIconReturns> = call(api, out, "GetTeamIcon", get(team)).await;
    for id in [team, MISSING] {
        let _: Option<Z_RemoveTeamIconReturns> = call(
            api,
            out,
            "RemoveTeamIcon",
            Z_RemoveTeamIconArgs { a: id.to_owned() },
        )
        .await;
    }
    // Go leaves the file where it was: the icon is still read after the removal.
    let _: Option<Z_GetTeamIconReturns> = call(api, out, "GetTeamIcon", get(team)).await;
    let _: Option<Z_GetTeamIconReturns> = call(api, out, "GetTeamIcon", get(MISSING)).await;
}

/// Users: the profile image (twice the same, so the second changes nothing), typing, toasts,
/// a push to a user with no device, LDAP attributes and a trial licence's refusals.
async fn users(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for (id, data) in [
        (input.own.as_str(), TINY_PNG.to_vec()),
        (input.own.as_str(), TINY_PNG.to_vec()),
        (input.own.as_str(), b"not an image".to_vec()),
        (MISSING, TINY_PNG.to_vec()),
    ] {
        let _: Option<Z_SetProfileImageReturns> = call(
            api,
            out,
            "SetProfileImage",
            Z_SetProfileImageArgs {
                a: id.to_owned(),
                b: data,
            },
        )
        .await;
    }

    let _: Option<Z_PublishUserTypingReturns> = call(
        api,
        out,
        "PublishUserTyping",
        Z_PublishUserTypingArgs {
            a: input.other.clone(),
            b: input.own_channel.clone(),
            c: String::new(),
        },
    )
    .await;

    for (user, message) in [
        (input.own.as_str(), "a toast"),
        ("", "a toast"),
        (input.own.as_str(), ""),
    ] {
        let _: Option<Z_SendToastMessageReturns> = call(
            api,
            out,
            "SendToastMessage",
            Z_SendToastMessageArgs {
                a: user.to_owned(),
                b: String::new(),
                c: message.to_owned(),
                d: SendToastMessageOptions {
                    position: "bottom-right".into(),
                },
            },
        )
        .await;
    }

    let _: Option<Z_SendPushNotificationReturns> = call(
        api,
        out,
        "SendPushNotification",
        Z_SendPushNotificationArgs {
            a: Some(Box::new(PushNotification {
                message: "a push".into(),
                channel_id: input.own_channel.clone(),
                ..PushNotification::default()
            })),
            b: input.own.clone(),
        },
    )
    .await;

    let _: Option<Z_GetLDAPUserAttributesReturns> = call(
        api,
        out,
        "GetLDAPUserAttributes",
        Z_GetLDAPUserAttributesArgs {
            a: input.own.clone(),
            b: vec!["cn".into()],
        },
    )
    .await;

    for (requester, users, terms) in [
        (input.own.as_str(), 10, false),
        (input.own.as_str(), 0, true),
        (MISSING, 10, true),
    ] {
        let _: Option<Z_RequestTrialLicenseReturns> = call(
            api,
            out,
            "RequestTrialLicense",
            Z_RequestTrialLicenseArgs {
                a: requester.to_owned(),
                b: users,
                c: terms,
                d: false,
            },
        )
        .await;
    }
}

/// Plugins: the installation's plugins and this one's status; the three writes only against an
/// id nothing has, since writing this plugin's own state would stop the script.
async fn plugins(api: &Client, out: &mut Vec<Json>) {
    let _: Option<Z_GetPluginsReturns> = call(api, out, "GetPlugins", Z_GetPluginsArgs {}).await;
    for id in ["mmrs.hookrecorder", "srv-no-such-plugin"] {
        let _: Option<Z_GetPluginStatusReturns> = call(
            api,
            out,
            "GetPluginStatus",
            Z_GetPluginStatusArgs { a: id.to_owned() },
        )
        .await;
    }
    let missing = "srv-no-such-plugin".to_owned();
    let _: Option<Z_EnablePluginReturns> = call(
        api,
        out,
        "EnablePlugin",
        Z_EnablePluginArgs { a: missing.clone() },
    )
    .await;
    let _: Option<Z_DisablePluginReturns> = call(
        api,
        out,
        "DisablePlugin",
        Z_DisablePluginArgs { a: missing.clone() },
    )
    .await;
    let _: Option<Z_RemovePluginReturns> =
        call(api, out, "RemovePlugin", Z_RemovePluginArgs { a: missing }).await;
}

fn command(input: &Inputs, trigger: &str, url: &str) -> Option<Box<Command>> {
    Some(Box::new(Command {
        team_id: input.side_team.clone(),
        trigger: trigger.to_owned(),
        method: "P".into(),
        url: url.to_owned(),
        display_name: "Srv".into(),
        creator_id: input.own.clone(),
        ..Command::default()
    }))
}

/// Commands: made (twice, the second a duplicate; once invalid), read, updated (under its own
/// id and a missing one), deleted, and read again.
async fn commands(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let trigger = format!("SrvCmd{}", input.side);
    let url = "http://localhost:9/srv";
    let made: Option<Z_CreateCommandReturns> = call(
        api,
        out,
        "CreateCommand",
        Z_CreateCommandArgs {
            a: command(input, &trigger, url),
        },
    )
    .await;
    let id = made.and_then(|m| m.a).map(|c| c.id).unwrap_or_default();
    for (trigger, url) in [(trigger.as_str(), url), ("srvinvalid", "")] {
        let _: Option<Z_CreateCommandReturns> = call(
            api,
            out,
            "CreateCommand",
            Z_CreateCommandArgs {
                a: command(input, trigger, url),
            },
        )
        .await;
    }
    for read in [id.as_str(), MISSING] {
        let _: Option<Z_GetCommandReturns> = call(
            api,
            out,
            "GetCommand",
            Z_GetCommandArgs { a: read.to_owned() },
        )
        .await;
    }
    // The plugin's creator is kept, so a command sent with one fails `IsValid` against the
    // plugin id the host sets; sent without, it saves, keeping its stored team.
    let mut update = command(input, &format!("SrvUpd{}", input.side), url);
    if let Some(c) = update.as_mut() {
        c.team_id.clear();
        c.description = "updated".into();
    }
    let mut without_creator = update.clone();
    if let Some(c) = without_creator.as_mut() {
        c.creator_id.clear();
    }
    for (target, body) in [
        (id.as_str(), update.clone()),
        (id.as_str(), without_creator.clone()),
        (MISSING, without_creator),
    ] {
        let _: Option<Z_UpdateCommandReturns> = call(
            api,
            out,
            "UpdateCommand",
            Z_UpdateCommandArgs {
                a: target.to_owned(),
                b: body,
            },
        )
        .await;
    }
    for target in [id.as_str(), MISSING] {
        let _: Option<Z_DeleteCommandReturns> = call(
            api,
            out,
            "DeleteCommand",
            Z_DeleteCommandArgs {
                a: target.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_GetCommandReturns> =
        call(api, out, "GetCommand", Z_GetCommandArgs { a: id }).await;
}

/// Upload sessions: made in the own channel, refused for a missing one, read, and read missing.
async fn uploads(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let session = |id: String, channel: &str| {
        Some(Box::new(UploadSession {
            id,
            r#type: "attachment".into(),
            user_id: input.own.clone(),
            channel_id: channel.to_owned(),
            filename: "srv.txt".into(),
            file_size: 10,
            ..UploadSession::default()
        }))
    };
    let id = input.upload_id();
    for (id, channel) in [
        (id.clone(), input.own_channel.as_str()),
        (id.replace("xxxxxxxx", "yyyyyyyy"), MISSING),
    ] {
        let _: Option<Z_CreateUploadSessionReturns> = call(
            api,
            out,
            "CreateUploadSession",
            Z_CreateUploadSessionArgs {
                a: session(id, channel),
            },
        )
        .await;
    }
    for read in [id.as_str(), MISSING] {
        let _: Option<Z_GetUploadSessionReturns> = call(
            api,
            out,
            "GetUploadSession",
            Z_GetUploadSessionArgs { a: read.to_owned() },
        )
        .await;
    }
}

/// The cluster, a collection registration, and two audit records: nothing to see but the answer.
async fn quiet(api: &Client, out: &mut Vec<Json>) {
    let _: Option<Z_PublishPluginClusterEventReturns> = call(
        api,
        out,
        "PublishPluginClusterEvent",
        Z_PublishPluginClusterEventArgs {
            a: PluginClusterEvent {
                id: "srv-event".into(),
                data: b"data".to_vec(),
            },
            b: PluginClusterEventSendOptions {
                send_type: "reliable".into(),
                target_id: String::new(),
            },
        },
    )
    .await;
    let _: Option<Z_RegisterCollectionAndTopicReturns> = call(
        api,
        out,
        "RegisterCollectionAndTopic",
        Z_RegisterCollectionAndTopicArgs {
            a: "srv-collection".into(),
            b: "srv-topic".into(),
        },
    )
    .await;
    let record = || {
        Some(Box::new(AuditRecord {
            event_name: "srvEvent".into(),
            status: "success".into(),
            ..AuditRecord::default()
        }))
    };
    let _: Option<Z_LogAuditRecReturns> =
        call(api, out, "LogAuditRec", Z_LogAuditRecArgs { a: record() }).await;
}

/// The whole script, in a fixed order.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    channels(api, input, &mut out).await;
    teams(api, input, &mut out).await;
    users(api, input, &mut out).await;
    plugins(api, &mut out).await;
    commands(api, input, &mut out).await;
    uploads(api, input, &mut out).await;
    quiet(api, &mut out).await;
    out
}
