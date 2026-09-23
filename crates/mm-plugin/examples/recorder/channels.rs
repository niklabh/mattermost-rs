//! The hook recorder's channels script: the plugin API's channel, member, sidebar, post-list,
//! reaction and emoji methods, each written down with what the host answered, for
//! `parity::plugin_hooks`' channels tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`CHANNELS_SCRIPT`], from inside `MessageWillBePosted`, like
//! the core script, and writes through the API as a plugin would: a channel it makes, edits,
//! fills, empties and archives; posts and reactions in the trigger's channel, which it then
//! pages through; the own user's sidebar; and a DM it cannot leave. Every hook those writes fire
//! lands in the transcript beside the script's entry.
//!
//! # What the suite hands it
//!
//! Through the environment: this side's **own** and **other** users, its **side team** and that
//! team's **town-square** and **off-topic**, its **side** tag, and two shared custom emoji both
//! sides only read. The own channel is the trigger's.
//!
//! # Two reads are filtered here
//!
//! `GetEmojiList` answers every emoji on the installation, which other suites change while this
//! one runs. Its answers are written down with only the two known emoji, **in the order the host
//! gave them**, so the order is still compared.

use go_netrpc::Client;
use mm_plugin::wire::model::{
    Channel, ChannelMemberIdentifier, Post, Reaction, SearchParameter, SearchParams,
    SidebarCategory, SidebarCategoryWithChannels,
};
use mm_plugin::wire::plugin::*;
use serde_json::{Value as Json, json};
use std::collections::HashMap;

use crate::core::{MISSING, call};
use crate::render::render_typed;

/// The message that runs the script.
pub const CHANNELS_SCRIPT: &str = "!channels-script";

/// What the suite put in the environment, and the trigger's channel.
pub struct Inputs {
    pub own: String,
    pub other: String,
    pub side_team: String,
    pub town: String,
    pub off: String,
    pub side: String,
    pub emoji_a: String,
    pub emoji_b: String,
    pub emoji_name: String,
    pub own_channel: String,
}

impl Inputs {
    /// The environment's half, with the trigger's channel.
    pub fn from_env(own_channel: &str) -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            own: var("HOOK_RECORDER_CHANNELS_OWN"),
            other: var("HOOK_RECORDER_CHANNELS_OTHER"),
            side_team: var("HOOK_RECORDER_CHANNELS_TEAM"),
            town: var("HOOK_RECORDER_CHANNELS_TOWN"),
            off: var("HOOK_RECORDER_CHANNELS_OFF"),
            side: var("HOOK_RECORDER_CHANNELS_SIDE"),
            emoji_a: var("HOOK_RECORDER_CHANNELS_EMOJI_A"),
            emoji_b: var("HOOK_RECORDER_CHANNELS_EMOJI_B"),
            emoji_name: var("HOOK_RECORDER_CHANNELS_EMOJI_NAME"),
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

fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// The reads of what the suite made, before the script writes anything.
async fn channel_reads(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for (team, deleted) in [(input.side_team.as_str(), false), (MISSING, false)] {
        let _: Option<Z_GetChannelsForTeamForUserReturns> = call(
            api,
            out,
            "GetChannelsForTeamForUser",
            Z_GetChannelsForTeamForUserArgs {
                a: team.to_owned(),
                b: input.own.clone(),
                c: deleted,
            },
        )
        .await;
    }
    // In display-name order; page 1 of 2 is the third of the three (offset `page * perPage`,
    // which an offset of `page` alone would answer with two).
    for (page, per_page) in [(0, 100), (1, 2), (0, 0)] {
        let _: Option<Z_GetPublicChannelsForTeamReturns> = call(
            api,
            out,
            "GetPublicChannelsForTeam",
            Z_GetPublicChannelsForTeamArgs {
                a: input.side_team.clone(),
                b: page,
                c: per_page,
            },
        )
        .await;
    }
    // The term is trimmed; the second matches nothing.
    for term in ["  chans  ", "nosuchpluginchannel"] {
        let _: Option<Z_SearchChannelsReturns> = call(
            api,
            out,
            "SearchChannels",
            Z_SearchChannelsArgs {
                a: input.side_team.clone(),
                b: term.to_owned(),
            },
        )
        .await;
    }
    for channel in [input.own_channel.as_str(), MISSING] {
        let _: Option<Z_GetChannelStatsReturns> = call(
            api,
            out,
            "GetChannelStats",
            Z_GetChannelStatsArgs {
                a: channel.to_owned(),
            },
        )
        .await;
    }
    for (page, per_page) in [(0, 100), (5, 100)] {
        let _: Option<Z_GetChannelMembersReturns> = call(
            api,
            out,
            "GetChannelMembers",
            Z_GetChannelMembersArgs {
                a: input.own_channel.clone(),
                b: page,
                c: per_page,
            },
        )
        .await;
    }
    for ids in [
        vec![input.own.clone(), MISSING.to_owned(), input.other.clone()],
        vec![],
    ] {
        let _: Option<Z_GetChannelMembersByIdsReturns> = call(
            api,
            out,
            "GetChannelMembersByIds",
            Z_GetChannelMembersByIdsArgs {
                a: input.own_channel.clone(),
                b: ids,
            },
        )
        .await;
    }
    // The team is ignored: a missing one lists the same memberships.
    for (team, page) in [(input.side_team.as_str(), 0), (MISSING, 0), ("", 1)] {
        let _: Option<Z_GetChannelMembersForUserReturns> = call(
            api,
            out,
            "GetChannelMembersForUser",
            Z_GetChannelMembersForUserArgs {
                a: team.to_owned(),
                b: input.own.clone(),
                c: page,
                d: 100,
            },
        )
        .await;
    }
}

/// A channel made, edited, filled, re-roled, muted, left and archived; a DM that cannot be left.
/// Answers the made channel.
async fn channel_writes(api: &Client, input: &Inputs, out: &mut Vec<Json>) -> Channel {
    let side = &input.side;
    let made: Option<Z_CreateChannelReturns> = call(
        api,
        out,
        "CreateChannel",
        Z_CreateChannelArgs {
            a: Some(Box::new(Channel {
                team_id: input.side_team.clone(),
                r#type: "O".into(),
                name: format!("plugchan-{side}"),
                display_name: format!("Plug Chan {side}"),
                ..Channel::default()
            })),
        },
    )
    .await;
    let made = made.and_then(|m| m.a).map(|c| *c).unwrap_or_default();

    // Edited whole: a new display name, header and purpose; then the same under a missing id,
    // and with a name the model refuses.
    let mut edited = made.clone();
    edited.display_name = format!("Plug Chan Renamed {side}");
    edited.header = "a plugin's header".into();
    edited.purpose = "a plugin's purpose".into();
    let answered: Option<Z_UpdateChannelReturns> = call(
        api,
        out,
        "UpdateChannel",
        Z_UpdateChannelArgs {
            a: Some(Box::new(edited.clone())),
        },
    )
    .await;
    let edited = answered.and_then(|a| a.a).map(|c| *c).unwrap_or(edited);
    for channel in [
        Channel {
            id: MISSING.into(),
            ..edited.clone()
        },
        Channel {
            name: String::new(),
            ..edited.clone()
        },
    ] {
        let _: Option<Z_UpdateChannelReturns> = call(
            api,
            out,
            "UpdateChannel",
            Z_UpdateChannelArgs {
                a: Some(Box::new(channel)),
            },
        )
        .await;
    }

    for user in [&input.own, &input.other] {
        let _: Option<Z_AddChannelMemberReturns> = call(
            api,
            out,
            "AddChannelMember",
            Z_AddChannelMemberArgs {
                a: made.id.clone(),
                b: user.clone(),
            },
        )
        .await;
    }
    let _: Option<Z_GetChannelStatsReturns> = call(
        api,
        out,
        "GetChannelStats",
        Z_GetChannelStatsArgs { a: made.id.clone() },
    )
    .await;
    for (user, roles) in [
        (input.other.as_str(), "channel_user channel_admin"),
        (input.other.as_str(), "channel_user nosuchpluginrole"),
        (input.other.as_str(), "system_admin"),
        (MISSING, "channel_user"),
    ] {
        let _: Option<Z_UpdateChannelMemberRolesReturns> = call(
            api,
            out,
            "UpdateChannelMemberRoles",
            Z_UpdateChannelMemberRolesArgs {
                a: made.id.clone(),
                b: user.to_owned(),
                c: roles.to_owned(),
            },
        )
        .await;
    }
    // Three known keys (one not a valid level, which is not checked here) and one unknown, which
    // is dropped; then a member who is not there.
    for (channel, user) in [
        (made.id.as_str(), input.own.as_str()),
        (made.id.as_str(), MISSING),
        (MISSING, input.own.as_str()),
    ] {
        let _: Option<Z_UpdateChannelMemberNotificationsReturns> = call(
            api,
            out,
            "UpdateChannelMemberNotifications",
            Z_UpdateChannelMemberNotificationsArgs {
                a: channel.to_owned(),
                b: user.to_owned(),
                c: props(&[
                    ("desktop", "mention"),
                    ("mark_unread", "all"),
                    ("push", "banana"),
                    ("nosuchpluginprop", "x"),
                ]),
            },
        )
        .await;
    }
    // Every refusal before the write: too many members, no props, an invalid level. Then a
    // patch of two memberships; then one naming a non-member, which Go answers with success and
    // rolls back — the read after shows the first patch's `desktop`, not this one's.
    let member = |channel: &str, user: &str| ChannelMemberIdentifier {
        channel_id: channel.to_owned(),
        user_id: user.to_owned(),
    };
    let own_member = member(&made.id, &input.own);
    for (members, notify) in [
        (vec![own_member.clone(); 201], props(&[("desktop", "all")])),
        (vec![own_member.clone()], HashMap::new()),
        (vec![own_member.clone()], props(&[("desktop", "banana")])),
        (
            vec![own_member.clone()],
            props(&[("mark_unread", "banana")]),
        ),
        (
            vec![own_member.clone(), member(&input.own_channel, &input.own)],
            props(&[("desktop", "none"), ("push", "mention")]),
        ),
        (
            vec![own_member, member(&made.id, MISSING)],
            props(&[("desktop", "all")]),
        ),
    ] {
        let _: Option<Z_PatchChannelMembersNotificationsReturns> = call(
            api,
            out,
            "PatchChannelMembersNotifications",
            Z_PatchChannelMembersNotificationsArgs {
                a: members,
                b: notify,
            },
        )
        .await;
    }
    let _: Option<Z_GetChannelMemberReturns> = call(
        api,
        out,
        "GetChannelMember",
        Z_GetChannelMemberArgs {
            a: made.id.clone(),
            b: input.own.clone(),
        },
    )
    .await;
    let _: Option<Z_GetChannelMembersForUserReturns> = call(
        api,
        out,
        "GetChannelMembersForUser",
        Z_GetChannelMembersForUserArgs {
            a: input.side_team.clone(),
            b: input.other.clone(),
            c: 0,
            d: 100,
        },
    )
    .await;
    let _: Option<Z_GetChannelMembersReturns> = call(
        api,
        out,
        "GetChannelMembers",
        Z_GetChannelMembersArgs {
            a: made.id.clone(),
            b: 0,
            c: 100,
        },
    )
    .await;

    // The other user leaves (a leave post), then cannot leave again; nobody leaves; nothing is
    // left; town-square cannot be left, and off-topic can (with its leave post).
    for (channel, user) in [
        (made.id.as_str(), input.other.as_str()),
        (made.id.as_str(), input.other.as_str()),
        (made.id.as_str(), MISSING),
        (MISSING, input.own.as_str()),
        (input.town.as_str(), input.own.as_str()),
        (input.off.as_str(), input.own.as_str()),
    ] {
        let _: Option<Z_DeleteChannelMemberReturns> = call(
            api,
            out,
            "DeleteChannelMember",
            Z_DeleteChannelMemberArgs {
                a: channel.to_owned(),
                b: user.to_owned(),
            },
        )
        .await;
    }
    // A DM cannot be left.
    let dm: Option<Z_GetDirectChannelReturns> = call(
        api,
        out,
        "GetDirectChannel",
        Z_GetDirectChannelArgs {
            a: input.own.clone(),
            b: input.other.clone(),
        },
    )
    .await;
    let dm = dm.and_then(|d| d.a).map(|c| c.id).unwrap_or_default();
    let _: Option<Z_DeleteChannelMemberReturns> = call(
        api,
        out,
        "DeleteChannelMember",
        Z_DeleteChannelMemberArgs {
            a: dm,
            b: input.own.clone(),
        },
    )
    .await;
    edited
}

/// The own user's sidebar: read, a category made and refused, an edit and an edit of nothing.
async fn sidebar(api: &Client, input: &Inputs, made: &Channel, out: &mut Vec<Json>) {
    read_sidebar(api, input, out).await;
    let category = |display_name: String, channels: Vec<String>| SidebarCategoryWithChannels {
        sidebar_category: SidebarCategory {
            display_name,
            ..SidebarCategory::default()
        },
        channels,
    };
    let made_category: Option<Z_CreateChannelSidebarCategoryReturns> = call(
        api,
        out,
        "CreateChannelSidebarCategory",
        Z_CreateChannelSidebarCategoryArgs {
            a: input.own.clone(),
            b: input.side_team.clone(),
            c: Some(Box::new(category(
                format!("Plug Cat {}", input.side),
                vec![input.own_channel.clone()],
            ))),
        },
    )
    .await;
    // A team the user has no categories on.
    let _: Option<Z_CreateChannelSidebarCategoryReturns> = call(
        api,
        out,
        "CreateChannelSidebarCategory",
        Z_CreateChannelSidebarCategoryArgs {
            a: input.own.clone(),
            b: MISSING.to_owned(),
            c: Some(Box::new(category("Nowhere".into(), vec![]))),
        },
    )
    .await;
    let mut renamed = made_category
        .and_then(|m| m.a)
        .map(|c| *c)
        .unwrap_or_default();
    renamed.sidebar_category.display_name = format!("Plug Cat Renamed {}", input.side);
    renamed.sidebar_category.collapsed = true;
    renamed.sidebar_category.sorting = "alpha".into();
    renamed.channels = vec![made.id.clone(), input.own_channel.clone()];
    let mut nobody = renamed.clone();
    nobody.sidebar_category.id = MISSING.into();
    for categories in [vec![renamed], vec![nobody]] {
        let _: Option<Z_UpdateChannelSidebarCategoriesReturns> = call(
            api,
            out,
            "UpdateChannelSidebarCategories",
            Z_UpdateChannelSidebarCategoriesArgs {
                a: input.own.clone(),
                b: input.side_team.clone(),
                c: categories,
            },
        )
        .await;
    }
    read_sidebar(api, input, out).await;
}

async fn read_sidebar(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let _: Option<Z_GetChannelSidebarCategoriesReturns> = call(
        api,
        out,
        "GetChannelSidebarCategories",
        Z_GetChannelSidebarCategoriesArgs {
            a: input.own.clone(),
            b: input.side_team.clone(),
        },
    )
    .await;
}

/// The made channel archived, refused a second time, a missing one and town-square; then the
/// listings that do and do not include it.
async fn archive(api: &Client, input: &Inputs, made: &Channel, out: &mut Vec<Json>) {
    for channel in [
        made.id.as_str(),
        made.id.as_str(),
        MISSING,
        input.town.as_str(),
    ] {
        let _: Option<Z_DeleteChannelReturns> = call(
            api,
            out,
            "DeleteChannel",
            Z_DeleteChannelArgs {
                a: channel.to_owned(),
            },
        )
        .await;
    }
    for deleted in [true, false] {
        let _: Option<Z_GetChannelsForTeamForUserReturns> = call(
            api,
            out,
            "GetChannelsForTeamForUser",
            Z_GetChannelsForTeamForUserArgs {
                a: input.side_team.clone(),
                b: input.own.clone(),
                c: deleted,
            },
        )
        .await;
    }
    let _: Option<Z_SearchChannelsReturns> = call(
        api,
        out,
        "SearchChannels",
        Z_SearchChannelsArgs {
            a: input.side_team.clone(),
            b: "plugchan".into(),
        },
    )
    .await;
}

/// Four posts, a reply and two reactions in the own channel; then every way of paging them.
async fn posts(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let side = &input.side;
    let mut ids = Vec::new();
    for message in [
        format!("plugpost one {side}"),
        format!("plugpost two {side}"),
        format!("plugpost three {side}"),
        format!("plugsearch four {side}"),
    ] {
        let made: Option<Z_CreatePostReturns> = call(
            api,
            out,
            "CreatePost",
            Z_CreatePostArgs {
                a: Some(Box::new(Post {
                    channel_id: input.own_channel.clone(),
                    user_id: input.own.clone(),
                    message,
                    ..Post::default()
                })),
            },
        )
        .await;
        ids.push(made.and_then(|m| m.a).map(|p| p.id).unwrap_or_default());
    }
    let reply: Option<Z_CreatePostReturns> = call(
        api,
        out,
        "CreatePost",
        Z_CreatePostArgs {
            a: Some(Box::new(Post {
                channel_id: input.own_channel.clone(),
                user_id: input.other.clone(),
                root_id: ids[0].clone(),
                message: format!("plugpost reply {side}"),
                ..Post::default()
            })),
        },
    )
    .await;
    let reply = reply.and_then(|m| m.a).map(|p| p.id).unwrap_or_default();
    let (one, two, four) = (ids[0].clone(), ids[1].clone(), ids[3].clone());

    for (user, emoji) in [(&input.own, "smile"), (&input.other, "thumbsup")] {
        let _: Option<Z_AddReactionReturns> = call(
            api,
            out,
            "AddReaction",
            Z_AddReactionArgs {
                a: Some(Box::new(Reaction {
                    user_id: user.clone(),
                    post_id: one.clone(),
                    emoji_name: emoji.to_owned(),
                    ..Reaction::default()
                })),
            },
        )
        .await;
    }
    for post in [one.as_str(), two.as_str(), MISSING] {
        let _: Option<Z_GetReactionsReturns> = call(
            api,
            out,
            "GetReactions",
            Z_GetReactionsArgs { a: post.to_owned() },
        )
        .await;
    }

    // Newest first: the reply, four, three | two, one. Page 2 of two would reach the suite's
    // join posts, so it is not read.
    for (channel, page, per_page) in [
        (input.own_channel.as_str(), 0, 3),
        (input.own_channel.as_str(), 1, 2),
        (input.own_channel.as_str(), 0, 1001),
        (MISSING, 0, 10),
    ] {
        let _: Option<Z_GetPostsForChannelReturns> = call(
            api,
            out,
            "GetPostsForChannel",
            Z_GetPostsForChannelArgs {
                a: channel.to_owned(),
                b: page,
                c: per_page,
            },
        )
        .await;
    }
    for (post, page, per_page) in [
        (one.as_str(), 0, 2),
        (one.as_str(), 1, 2),
        (one.as_str(), -1, 2),
        (one.as_str(), 0, -1),
        (MISSING, 0, 10),
    ] {
        let _: Option<Z_GetPostsAfterReturns> = call(
            api,
            out,
            "GetPostsAfter",
            Z_GetPostsAfterArgs {
                a: input.own_channel.clone(),
                b: post.to_owned(),
                c: page,
                d: per_page,
            },
        )
        .await;
    }
    for (post, page, per_page) in [
        (four.as_str(), 0, 2),
        (reply.as_str(), 1, 1),
        (four.as_str(), -1, 10),
        (four.as_str(), 0, -1),
    ] {
        let _: Option<Z_GetPostsBeforeReturns> = call(
            api,
            out,
            "GetPostsBefore",
            Z_GetPostsBeforeArgs {
                a: input.own_channel.clone(),
                b: post.to_owned(),
                c: page,
                d: per_page,
            },
        )
        .await;
    }
    // Since the second post's `UpdateAt`: everything written after it — three, four, the reply
    // and the thread's root, whose `UpdateAt` the reply and the reactions moved. The suite masks
    // this time. Then a time in the future, which is nothing.
    let two_updated = {
        let mut scratch = Vec::new();
        let read: Option<Z_GetPostReturns> = call(
            api,
            &mut scratch,
            "GetPost",
            Z_GetPostArgs { a: two.clone() },
        )
        .await;
        read.and_then(|r| r.a).map_or(0, |p| p.update_at)
    };
    for since in [two_updated, 4_102_444_800_000] {
        let _: Option<Z_GetPostsSinceReturns> = call(
            api,
            out,
            "GetPostsSince",
            Z_GetPostsSinceArgs {
                a: input.own_channel.clone(),
                b: since,
            },
        )
        .await;
    }

    for parameter in [
        SearchParameter {
            terms: Some("plugsearch".into()),
            ..SearchParameter::default()
        },
        SearchParameter {
            terms: Some("plugpost".into()),
            is_or_search: Some(true),
            page: Some(0),
            per_page: Some(1),
            ..SearchParameter::default()
        },
        SearchParameter {
            terms: Some("plugsearch".into()),
            page: Some(1),
            ..SearchParameter::default()
        },
        SearchParameter {
            terms: Some("*".into()),
            ..SearchParameter::default()
        },
        SearchParameter::default(),
    ] {
        let _: Option<Z_SearchPostsInTeamForUserReturns> = call(
            api,
            out,
            "SearchPostsInTeamForUser",
            Z_SearchPostsInTeamForUserArgs {
                a: input.side_team.clone(),
                b: input.own.clone(),
                c: parameter,
            },
        )
        .await;
    }
    let star = SearchParams {
        terms: "*".into(),
        ..SearchParams::default()
    };
    for params in [vec![star.clone(), star], vec![]] {
        let _: Option<Z_SearchPostsInTeamReturns> = call(
            api,
            out,
            "SearchPostsInTeam",
            Z_SearchPostsInTeamArgs {
                a: input.side_team.clone(),
                b: params,
            },
        )
        .await;
    }
}

/// The two shared emoji, by id, by name, listed both ways, and their image.
async fn emoji(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    for id in [input.emoji_a.as_str(), MISSING] {
        let _: Option<Z_GetEmojiReturns> =
            call(api, out, "GetEmoji", Z_GetEmojiArgs { a: id.to_owned() }).await;
    }
    for name in [input.emoji_name.as_str(), "nosuchpluginemoji", "smile"] {
        let _: Option<Z_GetEmojiByNameReturns> = call(
            api,
            out,
            "GetEmojiByName",
            Z_GetEmojiByNameArgs { a: name.to_owned() },
        )
        .await;
    }
    let known = [input.emoji_a.as_str(), input.emoji_b.as_str()];
    for sort in ["name", ""] {
        call_filtered::<_, Z_GetEmojiListReturns>(
            api,
            out,
            "GetEmojiList",
            Z_GetEmojiListArgs {
                a: sort.to_owned(),
                b: 0,
                c: 100_000,
            },
            &known,
        )
        .await;
    }
    let _: Option<Z_GetEmojiListReturns> = call(
        api,
        out,
        "GetEmojiList",
        Z_GetEmojiListArgs {
            a: "name".into(),
            b: 0,
            c: 0,
        },
    )
    .await;
    for id in [input.emoji_a.as_str(), MISSING] {
        let _: Option<Z_GetEmojiImageReturns> = call(
            api,
            out,
            "GetEmojiImage",
            Z_GetEmojiImageArgs { a: id.to_owned() },
        )
        .await;
    }
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    channel_reads(api, input, &mut out).await;
    let made = channel_writes(api, input, &mut out).await;
    sidebar(api, input, &made, &mut out).await;
    archive(api, input, &made, &mut out).await;
    posts(api, input, &mut out).await;
    emoji(api, input, &mut out).await;
    out
}
