//! The model values the plugin API's user, team, channel, post, bot and websocket methods carry
//! across gob (`crate::plugin_api`, docs/PLUGIN_PLAN.md Phase 6), in both directions.
//!
//! The hook conversions (`crate::plugin_hooks`) already cover users, channels, members, posts and
//! reactions going **out**; this adds the types only the API carries — teams, sessions, post
//! lists, bots, a permission, a websocket broadcast — and the ones that come **in** as an API
//! argument, where the rule differs from a hook's answer.
//!
//! # An argument is taken whole
//!
//! A hook's answer is merged over the value the host sent (Go decodes the reply into a seeded
//! struct), so an empty map in the answer means "unchanged". An API argument has nothing to be
//! merged over: Go's API server decodes it into a fresh `Z_<Method>Args`, so a map the plugin left
//! empty is a **nil** map on the Go side — `None` here, never `Some(empty)`.

use std::borrow::Cow;
use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use gobwire::Interface;
use mm_model::bot::{Bot, BotGetOptions, BotPatch};
use mm_model::custom_status::CustomStatus;
use mm_model::permission::Permission;
use mm_model::post_list::PostList;
use mm_model::preference::{Preference, Preferences};
use mm_model::reaction::Reaction;
use mm_model::session::Session;
use mm_model::stats::TeamStats;
use mm_model::status::Status;
use mm_model::team::Team;
use mm_model::team_member::TeamUnread;
use mm_model::utils::StringInterface;
use mm_model::utils::go_time;
use mm_model::websocket_message::WebsocketBroadcast;
use mm_plugin::wire::model as wire_model;

use crate::plugin_hooks::{post_to_wire, props_from_wire, string_map_to_wire, team_member_to_wire};

/// A team as gob sends it, all 22 fields of `model.Team` (team.go:28).
pub fn team_to_wire(team: &Team) -> wire_model::Team {
    wire_model::Team {
        id: team.id.clone(),
        create_at: team.create_at,
        update_at: team.update_at,
        delete_at: team.delete_at,
        display_name: team.display_name.clone(),
        name: team.name.clone(),
        description: team.description.clone(),
        email: team.email.clone(),
        r#type: team.team_type.clone(),
        company_name: team.company_name.clone(),
        allowed_domains: team.allowed_domains.clone(),
        invite_id: team.invite_id.clone(),
        allow_open_invite: team.allow_open_invite,
        last_team_icon_update: team.last_team_icon_update,
        scheme_id: team.scheme_id.clone(),
        group_constrained: team.group_constrained,
        policy_id: team.policy_id.clone(),
        cloud_limits_archived: team.cloud_limits_archived,
        policy_enforced: team.policy_enforced,
        policy_actions: team
            .policy_actions
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        policy_is_active: team.policy_is_active,
        recommended: team.recommended,
    }
}

/// A session as gob sends it (`model.Session`, session.go:39): **with its token**. `GetSession`
/// hands the plugin the store row, and gob carries `Token` like any other exported field.
pub fn session_to_wire(session: &Session) -> wire_model::Session {
    wire_model::Session {
        id: session.id.clone(),
        token: session.token.clone(),
        create_at: session.create_at,
        expires_at: session.expires_at,
        last_activity_at: session.last_activity_at,
        user_id: session.user_id.clone(),
        device_id: session.device_id.clone(),
        vo_ip_device_id: session.voip_device_id.clone(),
        roles: session.roles.clone(),
        is_o_auth: session.is_oauth,
        expired_notify: session.expired_notify,
        props: string_map_to_wire(session.props.as_ref()),
        team_members: session
            .team_members
            .iter()
            .flatten()
            .map(team_member_to_wire)
            .collect(),
        local: session.local,
    }
}

/// Port of `(*PostList).ForPlugin` (post_list.go:58) as gob sends the result: every post in
/// `Posts` passed through `ForPlugin`, the rest copied.
///
/// `BurnOnReadPosts` is copied **without** `ForPlugin` in Go — the one place a post could cross
/// with its metadata. [`post_to_wire`] never sends metadata, so such a post would arrive here
/// without it; the list is only non-empty with burn-on-read enabled, which this server refuses
/// on every read path (`App::prepare_post_list_for_client`).
pub fn post_list_for_plugin(list: &PostList) -> wire_model::PostList {
    let posts = |map: Option<&mm_model::post_list::PostMap>, for_plugin: bool| {
        map.into_iter()
            .flatten()
            .map(|(id, post)| {
                let post = if for_plugin {
                    post_to_wire(&post.for_plugin())
                } else {
                    post_to_wire(post)
                };
                (id.clone(), post)
            })
            .collect::<HashMap<_, _>>()
    };
    wire_model::PostList {
        order: list.order.clone().unwrap_or_default(),
        posts: posts(list.posts.as_ref(), true),
        next_post_id: list.next_post_id.clone(),
        prev_post_id: list.prev_post_id.clone(),
        has_next: list.has_next,
        first_inaccessible_post_time: list.first_inaccessible_post_time,
        burn_on_read_posts: posts(list.burn_on_read_posts.as_ref(), false),
    }
}

/// A bot as gob sends it (`model.Bot`, bot.go:28).
pub fn bot_to_wire(bot: &Bot) -> wire_model::Bot {
    wire_model::Bot {
        user_id: bot.user_id.clone(),
        username: bot.username.clone(),
        display_name: bot.display_name.clone(),
        description: bot.description.clone(),
        owner_id: bot.owner_id.clone(),
        last_icon_update: bot.last_icon_update,
        create_at: bot.create_at,
        update_at: bot.update_at,
        delete_at: bot.delete_at,
    }
}

/// A bot a plugin passed to `CreateBot` or `EnsureBotUser`.
pub fn bot_from_wire(wire: &wire_model::Bot) -> Bot {
    Bot {
        user_id: wire.user_id.clone(),
        username: wire.username.clone(),
        display_name: wire.display_name.clone(),
        description: wire.description.clone(),
        owner_id: wire.owner_id.clone(),
        last_icon_update: wire.last_icon_update,
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
    }
}

/// A `*model.BotPatch`: three pointers, which gob sends only when non-nil.
pub fn bot_patch_from_wire(wire: &wire_model::BotPatch) -> BotPatch {
    BotPatch {
        username: wire.username.clone(),
        display_name: wire.display_name.clone(),
        description: wire.description.clone(),
    }
}

/// A `*model.BotGetOptions`. Go's `Page` and `PerPage` are `int`; this store takes `i32`, so a
/// value outside it is clamped rather than wrapped — Go would compute an offset no database
/// could honour either way.
pub fn bot_get_options_from_wire(wire: &wire_model::BotGetOptions) -> BotGetOptions {
    let clamp = |n: i64| i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX });
    BotGetOptions {
        owner_id: wire.owner_id.clone(),
        include_deleted: wire.include_deleted,
        only_orphaned: wire.only_orphaned,
        page: clamp(wire.page),
        per_page: clamp(wire.per_page),
    }
}

/// A reaction a plugin passed to `AddReaction` or `RemoveReaction`.
pub fn reaction_from_wire(wire: &wire_model::Reaction) -> Reaction {
    Reaction {
        user_id: wire.user_id.clone(),
        post_id: wire.post_id.clone(),
        emoji_name: wire.emoji_name.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        remote_id: wire.remote_id.clone(),
        channel_id: wire.channel_id.clone(),
    }
}

/// A `*model.Permission`. Every check reads only `Id`; the rest is carried so that nothing is
/// invented.
pub fn permission_from_wire(wire: &wire_model::Permission) -> Permission {
    Permission {
        id: Cow::Owned(wire.id.clone()),
        name: Cow::Owned(wire.name.clone()),
        description: Cow::Owned(wire.description.clone()),
        scope: Cow::Owned(wire.scope.clone()),
    }
}

/// The `*model.WebsocketBroadcast` a plugin passed to `PublishWebSocketEvent`, taken whole.
///
/// A nil broadcast is Go's nil pointer, which `Publish` dereferences and panics on — the panic
/// takes down Go's API server with it. The zero broadcast (everyone) is what this answers
/// instead; no plugin can depend on the Go behaviour.
pub fn broadcast_from_wire(wire: Option<&wire_model::WebsocketBroadcast>) -> WebsocketBroadcast {
    let Some(wire) = wire else {
        return WebsocketBroadcast::default();
    };
    WebsocketBroadcast {
        omit_users: (!wire.omit_users.is_empty()).then(|| {
            wire.omit_users
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        }),
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        team_id: wire.team_id.clone(),
        connection_id: wire.connection_id.clone(),
        omit_connection_id: wire.omit_connection_id.clone(),
        contains_sanitized_data: wire.contains_sanitized_data,
        contains_sensitive_data: wire.contains_sensitive_data,
        reliable_cluster_send: wire.reliable_cluster_send,
        broadcast_hooks: (!wire.broadcast_hooks.is_empty()).then(|| wire.broadcast_hooks.clone()),
        broadcast_hook_args: (!wire.broadcast_hook_args.is_empty()).then(|| {
            wire.broadcast_hook_args
                .iter()
                .map(props_from_wire)
                .collect()
        }),
    }
}

/// The `map[string]any` payload of `PublishWebSocketEvent` as the event's data. A value crosses
/// gob as the type Go registered for it, and marshals as `encoding/json` writes that type: an
/// integral `float64` without a fraction, which [`props_from_wire`] already does.
pub fn payload_from_wire(payload: &HashMap<String, Option<Interface>>) -> StringInterface {
    props_from_wire(payload)
}

/// The event name `PublishWebSocketEvent` publishes under (plugin_api.go:1241):
/// `custom_<plugin id>_<event>`, with the manifest's id as it is spelled — not lowercased.
pub fn custom_event_name(plugin_id: &str, event: &str) -> String {
    format!("custom_{plugin_id}_{event}")
}

/// A team a plugin passed to `CreateTeam` or `UpdateTeam`, taken whole: gob's omitted empty map
/// is Go's nil `PolicyActions`, and the three pointers are carried as they came.
pub fn team_from_wire(wire: &wire_model::Team) -> Team {
    Team {
        id: wire.id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        display_name: wire.display_name.clone(),
        name: wire.name.clone(),
        description: wire.description.clone(),
        email: wire.email.clone(),
        team_type: wire.r#type.clone(),
        company_name: wire.company_name.clone(),
        allowed_domains: wire.allowed_domains.clone(),
        invite_id: wire.invite_id.clone(),
        allow_open_invite: wire.allow_open_invite,
        last_team_icon_update: wire.last_team_icon_update,
        scheme_id: wire.scheme_id.clone(),
        group_constrained: wire.group_constrained,
        policy_id: wire.policy_id.clone(),
        cloud_limits_archived: wire.cloud_limits_archived,
        policy_enforced: wire.policy_enforced,
        policy_actions: (!wire.policy_actions.is_empty()).then(|| {
            wire.policy_actions
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        }),
        policy_is_active: wire.policy_is_active,
        recommended: wire.recommended,
    }
}

/// A status as gob sends it (`model.Status`, status.go:24): **with** `PrevStatus` and
/// `ActiveChannel`, which JSON hides (`json:"-"`, `omitempty`) and gob carries like any other
/// exported field.
pub fn status_to_wire(status: &Status) -> wire_model::Status {
    wire_model::Status {
        user_id: status.user_id.clone(),
        status: status.status.clone(),
        manual: status.manual,
        last_activity_at: status.last_activity_at,
        active_channel: status.active_channel.clone(),
        dnd_end_time: status.dnd_end_time,
        prev_status: status.prev_status.clone(),
    }
}

/// A preference as gob sends it (`model.Preference`, preference.go:69).
pub fn preference_to_wire(preference: &Preference) -> wire_model::Preference {
    wire_model::Preference {
        user_id: preference.user_id.clone(),
        category: preference.category.clone(),
        name: preference.name.clone(),
        value: preference.value.clone(),
    }
}

/// The preferences a plugin passed to `UpdatePreferencesForUser` or `DeletePreferencesForUser`.
pub fn preferences_from_wire(wire: &[wire_model::Preference]) -> Preferences {
    Preferences(
        wire.iter()
            .map(|p| Preference {
                user_id: p.user_id.clone(),
                category: p.category.clone(),
                name: p.name.clone(),
                value: p.value.clone(),
            })
            .collect(),
    )
}

/// A team's unread counts as gob sends them (`model.TeamUnread`, team.go:70).
pub fn team_unread_to_wire(unread: &TeamUnread) -> wire_model::TeamUnread {
    wire_model::TeamUnread {
        team_id: unread.team_id.clone(),
        msg_count: unread.msg_count,
        mention_count: unread.mention_count,
        mention_count_root: unread.mention_count_root,
        msg_count_root: unread.msg_count_root,
        thread_count: unread.thread_count,
        thread_mention_count: unread.thread_mention_count,
        thread_urgent_mention_count: unread.thread_urgent_mention_count,
    }
}

/// A team's member counts as gob sends them (`model.TeamStats`, team_stats.go:6).
pub fn team_stats_to_wire(stats: &TeamStats) -> wire_model::TeamStats {
    wire_model::TeamStats {
        team_id: stats.team_id.clone(),
        total_member_count: stats.total_member_count,
        active_member_count: stats.active_member_count,
    }
}

/// The `*model.CustomStatus` a plugin passed to `UpdateUserCustomStatus`. `ExpiresAt` is a
/// `time.Time`, which crosses gob as its binary form: seconds, nanoseconds and an offset. The
/// offset survives, so the stored JSON prints the instant in the zone the plugin wrote it in,
/// as Go's does; the zero time (omitted by gob) is Go's zero time.
pub fn custom_status_from_wire(wire: &wire_model::CustomStatus) -> CustomStatus {
    CustomStatus {
        emoji: wire.emoji.clone(),
        text: wire.text.clone(),
        duration: wire.duration.clone(),
        expires_at: go_time_to_chrono(&wire.expires_at),
    }
}

/// A gob `time.Time` as the model's `DateTime<FixedOffset>`: the zero time is Go's, and an
/// instant chrono cannot hold (beyond ±262,000 years) is taken as the zero time too.
pub fn go_time_to_chrono(time: &gobwire::GoTime) -> DateTime<FixedOffset> {
    if *time == gobwire::GoTime::default() {
        return *go_time::ZERO;
    }
    let offset = match time.zone {
        gobwire::Zone::Utc => 0,
        gobwire::Zone::Offset(seconds) => seconds,
    };
    let nanos = u32::try_from(time.nsec).unwrap_or(0);
    match (
        FixedOffset::east_opt(offset),
        DateTime::from_timestamp(time.unix(), nanos),
    ) {
        (Some(zone), Some(utc)) => utc.with_timezone(&zone),
        _ => *go_time::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_custom_event_name_keeps_the_plugin_id_as_spelled() {
        assert_eq!(
            custom_event_name("com.Example.Plugin", "tick"),
            "custom_com.Example.Plugin_tick"
        );
        assert_eq!(custom_event_name("p", ""), "custom_p_");
    }

    /// Nil pieces stay nil: an empty map or list from gob is Go's nil, which `WebsocketBroadcast`
    /// models as `None`.
    #[test]
    fn a_broadcast_is_taken_whole_with_empty_as_nil() {
        let wire = wire_model::WebsocketBroadcast {
            omit_users: HashMap::from([("u1".to_owned(), true)]),
            user_id: "u".into(),
            channel_id: "c".into(),
            team_id: "t".into(),
            connection_id: "conn".into(),
            omit_connection_id: "omit".into(),
            contains_sanitized_data: true,
            contains_sensitive_data: false,
            reliable_cluster_send: true,
            ..Default::default()
        };
        let broadcast = broadcast_from_wire(Some(&wire));
        assert_eq!(
            broadcast.omit_users,
            Some([("u1".to_owned(), true)].into_iter().collect())
        );
        assert_eq!(
            (
                broadcast.user_id.as_str(),
                broadcast.channel_id.as_str(),
                broadcast.team_id.as_str(),
                broadcast.connection_id.as_str(),
                broadcast.omit_connection_id.as_str(),
            ),
            ("u", "c", "t", "conn", "omit")
        );
        assert!(broadcast.contains_sanitized_data);
        assert!(!broadcast.contains_sensitive_data);
        assert!(broadcast.reliable_cluster_send);
        assert_eq!(broadcast.broadcast_hooks, None);
        assert_eq!(broadcast.broadcast_hook_args, None);

        let empty = broadcast_from_wire(Some(&wire_model::WebsocketBroadcast::default()));
        assert_eq!(empty.omit_users, None);
        assert_eq!(broadcast_from_wire(None), WebsocketBroadcast::default());
    }

    /// An integral float is written as Go writes it, and a nested document survives.
    #[test]
    fn the_payload_is_json_as_go_marshals_it() {
        let payload = HashMap::from([
            ("n".to_owned(), Some(Interface::float64(2.0))),
            ("f".to_owned(), Some(Interface::float64(1.5))),
            ("i".to_owned(), Some(Interface::int(7))),
            ("s".to_owned(), Some(Interface::string("x"))),
            ("b".to_owned(), Some(Interface::bool(true))),
            ("nil".to_owned(), None),
        ]);
        let data = serde_json::Value::Object(payload_from_wire(&payload));
        assert_eq!(
            data.to_string(),
            r#"{"b":true,"f":1.5,"i":7,"n":2,"nil":null,"s":"x"}"#
        );
    }

    #[test]
    fn bot_options_clamp_rather_than_wrap() {
        let options = bot_get_options_from_wire(&wire_model::BotGetOptions {
            owner_id: "o".into(),
            include_deleted: true,
            only_orphaned: true,
            page: i64::from(i32::MAX) + 1,
            per_page: -5,
        });
        assert_eq!(options.page, i32::MAX);
        assert_eq!(options.per_page, -5);
        assert!(options.include_deleted && options.only_orphaned);
        let low = bot_get_options_from_wire(&wire_model::BotGetOptions {
            page: i64::MIN,
            ..Default::default()
        });
        assert_eq!(low.page, i32::MIN);
    }

    /// `ForPlugin` drops each post's metadata and the up-notification's `requested_features`;
    /// the list's own fields are copied.
    #[test]
    fn a_post_list_crosses_for_plugin() {
        let mut post = mm_model::post::Post {
            id: "p1".into(),
            post_type: "custom_up_notification".into(),
            metadata: Some(mm_model::post_metadata::PostMetadata::default()),
            ..Default::default()
        };
        post.add_prop("requested_features", serde_json::json!({"a": 1}));
        post.add_prop("kept", serde_json::json!("yes"));
        let list = PostList {
            order: Some(vec!["p1".into()]),
            posts: Some([("p1".to_owned(), post)].into_iter().collect()),
            next_post_id: "n".into(),
            prev_post_id: "p".into(),
            has_next: Some(false),
            first_inaccessible_post_time: 9,
            burn_on_read_posts: None,
        };
        let wire = post_list_for_plugin(&list);
        assert_eq!(wire.order, vec!["p1".to_owned()]);
        let crossed = &wire.posts["p1"];
        assert!(crossed.metadata.is_none());
        assert!(!crossed.props.contains_key("requested_features"));
        assert!(crossed.props.contains_key("kept"));
        assert_eq!(
            (wire.next_post_id.as_str(), wire.prev_post_id.as_str()),
            ("n", "p")
        );
        assert_eq!(wire.has_next, Some(false));
        assert_eq!(wire.first_inaccessible_post_time, 9);
        assert!(wire.burn_on_read_posts.is_empty());
    }

    #[test]
    fn a_session_crosses_with_its_token() {
        let session = Session {
            id: "s".into(),
            token: "t".into(),
            user_id: "u".into(),
            roles: "system_user".into(),
            is_oauth: true,
            props: Some(
                [("os".to_owned(), "Linux".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };
        let wire = session_to_wire(&session);
        assert_eq!(wire.token, "t");
        assert!(wire.is_o_auth);
        assert_eq!(wire.props.get("os").map(String::as_str), Some("Linux"));
        assert!(wire.team_members.is_empty());
    }

    #[test]
    fn a_bot_round_trips() {
        let bot = Bot {
            user_id: "u".into(),
            username: "b".into(),
            display_name: "d".into(),
            description: "x".into(),
            owner_id: "o".into(),
            last_icon_update: 1,
            create_at: 2,
            update_at: 3,
            delete_at: 4,
        };
        assert_eq!(bot_from_wire(&bot_to_wire(&bot)), bot);
    }

    /// Every field distinct and non-zero, so a swapped pair or a dropped field shows.
    #[test]
    fn a_team_round_trips_and_an_empty_policy_map_is_nil() {
        let team = Team {
            id: "i".into(),
            create_at: 1,
            update_at: 2,
            delete_at: 3,
            display_name: "dn".into(),
            name: "n".into(),
            description: "de".into(),
            email: "e@x.invalid".into(),
            team_type: "I".into(),
            company_name: "c".into(),
            allowed_domains: "a.invalid".into(),
            invite_id: "inv".into(),
            allow_open_invite: true,
            last_team_icon_update: 4,
            scheme_id: Some("s".into()),
            group_constrained: Some(false),
            policy_id: Some("p".into()),
            cloud_limits_archived: true,
            policy_enforced: true,
            policy_actions: Some(HashMap::from([("membership".to_owned(), true)])),
            policy_is_active: true,
            recommended: true,
        };
        assert_eq!(team_from_wire(&team_to_wire(&team)), team);
        let bare = Team {
            policy_actions: Some(HashMap::new()),
            ..team
        };
        assert_eq!(team_from_wire(&team_to_wire(&bare)).policy_actions, None);
    }

    /// `PrevStatus` and `ActiveChannel` are hidden from JSON and carried by gob.
    #[test]
    fn a_status_crosses_with_the_fields_json_hides() {
        let status = Status {
            user_id: "u".into(),
            status: "dnd".into(),
            manual: true,
            last_activity_at: 5,
            active_channel: "c".into(),
            dnd_end_time: 6,
            prev_status: "away".into(),
        };
        let wire = status_to_wire(&status);
        assert_eq!(
            (
                wire.user_id.as_str(),
                wire.status.as_str(),
                wire.manual,
                wire.last_activity_at,
                wire.active_channel.as_str(),
                wire.dnd_end_time,
                wire.prev_status.as_str()
            ),
            ("u", "dnd", true, 5, "c", 6, "away")
        );
    }

    #[test]
    fn preferences_and_counts_cross_field_for_field() {
        let preference = Preference {
            user_id: "u".into(),
            category: "c".into(),
            name: "n".into(),
            value: "v".into(),
        };
        let wire = preference_to_wire(&preference);
        assert_eq!(preferences_from_wire(&[wire]).0, vec![preference]);

        let unread = TeamUnread {
            team_id: "t".into(),
            msg_count: 1,
            mention_count: 2,
            mention_count_root: 3,
            msg_count_root: 4,
            thread_count: 5,
            thread_mention_count: 6,
            thread_urgent_mention_count: 7,
        };
        let wire = team_unread_to_wire(&unread);
        assert_eq!(
            [
                wire.msg_count,
                wire.mention_count,
                wire.mention_count_root,
                wire.msg_count_root,
                wire.thread_count,
                wire.thread_mention_count,
                wire.thread_urgent_mention_count
            ],
            [1, 2, 3, 4, 5, 6, 7]
        );
        let stats = team_stats_to_wire(&TeamStats {
            team_id: "t".into(),
            total_member_count: 8,
            active_member_count: 9,
        });
        assert_eq!(
            (
                stats.team_id.as_str(),
                stats.total_member_count,
                stats.active_member_count
            ),
            ("t", 8, 9)
        );
    }

    /// The zero `time.Time` is Go's zero; any other keeps its instant, nanoseconds and offset.
    #[test]
    fn a_gob_time_keeps_its_instant_and_its_offset() {
        assert!(go_time::is_zero(&go_time_to_chrono(
            &gobwire::GoTime::default()
        )));
        let ist =
            gobwire::GoTime::from_unix(1_900_000_000, 250_000_000, gobwire::Zone::Offset(19_800));
        let time = go_time_to_chrono(&ist);
        assert_eq!(time.timestamp(), 1_900_000_000);
        assert_eq!(time.timestamp_subsec_nanos(), 250_000_000);
        assert_eq!(time.offset().local_minus_utc(), 19_800);
        let utc = go_time_to_chrono(&gobwire::GoTime::from_unix(60, 0, gobwire::Zone::Utc));
        assert_eq!((utc.timestamp(), utc.offset().local_minus_utc()), (60, 0));

        let status = custom_status_from_wire(&wire_model::CustomStatus {
            emoji: "e".into(),
            text: "t".into(),
            duration: "d".into(),
            expires_at: ist,
        });
        assert_eq!(
            (
                status.emoji.as_str(),
                status.text.as_str(),
                status.duration.as_str()
            ),
            ("e", "t", "d")
        );
        assert_eq!(status.expires_at, time);
    }
}
