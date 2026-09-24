//! Cross-server parity for the three branches of `removeUserFromChannel` (app/channel.go:2999)
//! that used to forward the whole request ([D-1130]): a **guest** leaving their last channel on a
//! team, a **group-constrained** channel swept by somebody else, and a **shared** channel.
//!
//! ```sh
//! scripts/parity.sh --test parity channel_member_removal
//! ```
//!
//! Every case builds its own team, channel and users on each server, because each one is a
//! one-way write. The two servers' results are reduced to a summary with the ids replaced by
//! their role (`<guest>`, `<team>`, …) and the summaries compared, so a mismatch names the fact
//! that differs rather than an id.
//!
//! The guest is made by SQL, as `parity::websocket_guests` makes one: the three role columns, on
//! the unlicensed pair. Nothing in `removeUserFromChannel` asks about the licence or
//! `GuestAccountsSettings`, and the token minted before the role change is still a non-guest
//! session, so it can hold a socket open to hear the events addressed to the guest.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, add_user_to_channel, assert_error_bodies_match_except_known_gaps,
    client, create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    invalidate_go_caches, logged_in_user_id, stack_enabled,
};

/// The licensed mm-api with `EnableSharedChannels` on; see `second_server_ports`.
const SHARED_RUST_PORT: u16 = 8120;

/// One request, returning `(status, served here, body)`.
async fn send(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, bool, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, served, response.text().await.unwrap_or_default())
}

/// `ids` replaced by their names wherever they occur in `value`, recursively — keys included,
/// since `omit_users` is keyed by user id.
fn named(value: &serde_json::Value, ids: &[(&str, &str)]) -> serde_json::Value {
    let rename = |text: &str| -> String {
        ids.iter()
            .fold(text.to_owned(), |acc, (id, name)| acc.replace(id, name))
    };
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(rename(text)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| named(item, ids)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, inner)| (rename(key), named(inner, ids)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The frames of the named events that mention any of `ids` but the admin's, renamed, without the per-connection
/// `seq`, and sorted — Go publishes through a hub goroutine and neither server promises the order
/// two independent events reach one socket in.
fn events(probe: &SocketProbe, names: &[&str], ids: &[(&str, &str)]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .flat_map(|name| probe.events_named(name))
        .filter(|frame| {
            let text = frame.to_string();
            // The admin is every other suite's remover too, so naming them proves nothing.
            ids.iter()
                .any(|(id, name)| *name != "<admin>" && text.contains(id))
        })
        .map(|mut frame| {
            if let Some(object) = frame.as_object_mut() {
                object.remove("seq");
            }
            named(&frame, ids).to_string()
        })
        .collect();
    out.sort();
    out
}

async fn scalar(pool: &sqlx::PgPool, sql: &str, args: &[&str]) -> i64 {
    let mut query = sqlx::query_scalar::<_, i64>(sql);
    for arg in args {
        query = query.bind(*arg);
    }
    query
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Make `user_id` a guest in the three places the role lives.
async fn make_guest(pool: &sqlx::PgPool, user_id: &str) {
    for statement in [
        "UPDATE users SET roles = 'system_guest' WHERE id = $1",
        "UPDATE teammembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
        "UPDATE channelmembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("the guest fixture is written");
    }
}

/// Drop every channel membership `user_id` holds on `team_id` except `keep`.
async fn keep_only(pool: &sqlx::PgPool, user_id: &str, team_id: &str, keep: &[&str]) {
    let keep: Vec<String> = keep.iter().map(|id| (*id).to_owned()).collect();
    sqlx::query(
        "DELETE FROM channelmembers WHERE userid = $1
            AND channelid IN (SELECT id FROM channels WHERE teamid = $2)
            AND NOT (channelid = ANY($3))",
    )
    .bind(user_id)
    .bind(team_id)
    .bind(&keep)
    .execute(pool)
    .await
    .expect("the other memberships are dropped");
}

/// A copy of `user_id`'s membership of `from`, as a membership of `to` — how a user is put in a
/// channel no route will add them to (a space's backing channel).
async fn copy_membership(pool: &sqlx::PgPool, user_id: &str, from: &str, to: &str) {
    sqlx::query(
        "INSERT INTO channelmembers
         SELECT (jsonb_populate_record(NULL::channelmembers,
                    to_jsonb(cm) || jsonb_build_object('channelid', $3::text))).*
           FROM channelmembers cm
          WHERE cm.userid = $1 AND cm.channelid = $2",
    )
    .bind(user_id)
    .bind(from)
    .bind(to)
    .execute(pool)
    .await
    .expect("the membership is copied");
}

/// What one guest removal left behind, with every id replaced by its role.
#[derive(Debug, PartialEq)]
struct GuestRemoval {
    status: u16,
    body: String,
    on_team: bool,
    channels_on_team: i64,
    sidebar_categories: i64,
    team_preferences: i64,
    update_at_bumped: bool,
    removal_posts: i64,
    team_posts: i64,
    admin_heard: Vec<String>,
    guest_heard: Vec<String>,
}

/// A guest on a fresh team, a member of `channel` and of nothing else there but `also` (channels
/// the caller names), with a sidebar and a team-category preference for the leave to clear.
struct GuestFixture {
    team: String,
    channel: String,
    guest: common::PlainUser,
}

async fn guest_fixture(
    http: &reqwest::Client,
    pool: &sqlx::PgPool,
    admin: &str,
    tag: &str,
) -> GuestFixture {
    let team = create_team(http, admin, tag).await;
    let channel = create_channel_typed(http, admin, &team, tag, "O").await;
    let guest = create_plain_user(http, admin, &team, tag).await;
    add_user_to_channel(http, admin, &channel, &guest.id).await;
    // The categories exist once they have been asked for.
    let (status, _, body) = send(
        http,
        GO,
        reqwest::Method::GET,
        &format!(
            "/api/v4/users/{}/teams/{team}/channels/categories",
            guest.id
        ),
        &guest.token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the guest's categories: {body}");
    sqlx::query(
        "INSERT INTO preferences (userid, category, name, value) VALUES ($1, $2, 'mmrs', 'x')",
    )
    .bind(&guest.id)
    .bind(&team)
    .execute(pool)
    .await
    .expect("a team-category preference");
    make_guest(pool, &guest.id).await;
    GuestFixture {
        team,
        channel,
        guest,
    }
}

async fn remove_guest(
    http: &reqwest::Client,
    pool: &sqlx::PgPool,
    base: &str,
    admin: &str,
    fixture: &GuestFixture,
    channel: &str,
) -> GuestRemoval {
    let guest = &fixture.guest;
    let team = &fixture.team;
    let me = logged_in_user_id();
    let update_at_before = scalar(
        pool,
        "SELECT updateat FROM users WHERE id = $1",
        &[&guest.id],
    )
    .await;

    let mut admin_socket = SocketProbe::connect(base, admin).await;
    let mut guest_socket = SocketProbe::connect(base, &guest.token).await;
    let path = format!("/api/v4/channels/{channel}/members/{}", guest.id);
    let (status, served, body) =
        send(http, base, reqwest::Method::DELETE, &path, admin, None).await;
    assert_eq!(served, base == RUST, "{base}: served where it was sent");
    admin_socket.collect_for(Duration::from_millis(900)).await;
    guest_socket.collect_for(Duration::from_millis(900)).await;

    let ids = [
        (guest.id.as_str(), "<guest>"),
        (team.as_str(), "<team>"),
        (channel, "<channel>"),
        (me, "<admin>"),
    ];
    let names = ["user_removed", "leave_team"];
    GuestRemoval {
        status,
        body,
        on_team: scalar(
            pool,
            "SELECT COUNT(*) FROM teammembers WHERE teamid = $1 AND userid = $2 AND deleteat = 0",
            &[team, &guest.id],
        )
        .await
            == 1,
        channels_on_team: scalar(
            pool,
            "SELECT COUNT(*) FROM channelmembers cm JOIN channels c ON c.id = cm.channelid
              WHERE c.teamid = $1 AND cm.userid = $2",
            &[team, &guest.id],
        )
        .await,
        sidebar_categories: scalar(
            pool,
            "SELECT COUNT(*) FROM sidebarcategories WHERE userid = $1 AND teamid = $2",
            &[&guest.id, team],
        )
        .await,
        team_preferences: scalar(
            pool,
            "SELECT COUNT(*) FROM preferences WHERE userid = $1 AND category = $2",
            &[&guest.id, team],
        )
        .await,
        update_at_bumped: scalar(pool, "SELECT updateat FROM users WHERE id = $1", &[&guest.id])
            .await
            > update_at_before,
        removal_posts: scalar(
            pool,
            "SELECT COUNT(*) FROM posts WHERE channelid = $1 AND type = 'system_remove_from_channel'",
            &[channel],
        )
        .await,
        team_posts: scalar(
            pool,
            "SELECT COUNT(*) FROM posts p JOIN channels c ON c.id = p.channelid
              WHERE c.teamid = $1 AND p.type IN ('system_leave_team', 'system_remove_from_team')",
            &[team],
        )
        .await,
        admin_heard: events(&admin_socket, &names, &ids),
        guest_heard: events(&guest_socket, &names, &ids),
    }
}

/// A guest removed from their last channel on a team is removed from the team: the membership
/// soft-deleted, the sidebar and the team's preference category cleared, `UpdateAt` bumped,
/// both `leave_team` events published — and, unlike `LeaveTeam`, no team-leave post.
#[tokio::test]
async fn a_guests_last_channel_takes_them_off_the_team() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;

    let mut results = Vec::new();
    for (base, tag) in [(GO, "rmglgo"), (RUST, "rmglrs")] {
        let fixture = guest_fixture(&http, &pool, &admin, tag).await;
        keep_only(&pool, &fixture.guest.id, &fixture.team, &[&fixture.channel]).await;
        invalidate_go_caches(&http, &admin).await;
        results.push(remove_guest(&http, &pool, base, &admin, &fixture, &fixture.channel).await);
        common::delete_plain_user(&http, &admin, &fixture.guest.id).await;
    }
    let rust = results.pop().expect("two results");
    let go = results.pop().expect("two results");
    assert_eq!(go.status, 200, "Go removed the guest: {go:?}");
    assert!(!go.on_team, "Go took the guest off the team: {go:?}");
    assert_eq!(go.sidebar_categories, 0, "{go:?}");
    assert_eq!(go.team_preferences, 0, "{go:?}");
    assert!(
        go.admin_heard.iter().any(|e| e.contains("leave_team")),
        "the team heard the guest leave: {go:?}"
    );
    assert_eq!(rust, go);
}

/// A guest with another channel on the team stays on it, and so does a guest whose only other
/// membership is a **space** channel — which `GetChannelMembersForUser` cannot see, so the second
/// read is what keeps them.
#[tokio::test]
async fn a_guest_with_another_channel_or_a_space_stays_on_the_team() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;

    let mut results = Vec::new();
    for (base, tag) in [(GO, "rmgsgo"), (RUST, "rmgsrs")] {
        let fixture = guest_fixture(&http, &pool, &admin, tag).await;
        let second =
            create_channel_typed(&http, &admin, &fixture.team, &format!("{tag}2"), "O").await;
        add_user_to_channel(&http, &admin, &second, &fixture.guest.id).await;
        let space = common::plant_channel_of_type(&fixture.team, "S", &format!("{tag}s"))
            .await
            .expect("the space channel");
        copy_membership(&pool, &fixture.guest.id, &fixture.channel, &space).await;
        keep_only(
            &pool,
            &fixture.guest.id,
            &fixture.team,
            &[&fixture.channel, &second, &space],
        )
        .await;
        invalidate_go_caches(&http, &admin).await;

        // Another channel left: the first read keeps them.
        let first = remove_guest(&http, &pool, base, &admin, &fixture, &fixture.channel).await;
        // Only the space left: the second read keeps them.
        let then = remove_guest(&http, &pool, base, &admin, &fixture, &second).await;
        results.push((first, then));
        common::delete_plain_user(&http, &admin, &fixture.guest.id).await;
    }
    let rust = results.pop().expect("two results");
    let go = results.pop().expect("two results");
    for step in [&go.0, &go.1] {
        assert_eq!(step.status, 200, "{step:?}");
        assert!(step.on_team, "Go kept the guest on the team: {step:?}");
        assert!(
            !step.admin_heard.iter().any(|e| e.contains("leave_team")),
            "{step:?}"
        );
    }
    assert_eq!(go.1.channels_on_team, 1, "only the space is left");
    assert_eq!(rust, go);
}

/// Plant a group linked to `channel` with `members` in it, the link, group and memberships each
/// live or deleted as asked.
async fn plant_group(
    pool: &sqlx::PgPool,
    tag: &str,
    channel: &str,
    members: &[&str],
    link_deleted: bool,
    group_deleted: bool,
    membership_deleted: bool,
) {
    let id = format!("mmrsrmgrp{tag:0>17}");
    for statement in [
        "DELETE FROM groupmembers WHERE groupid = $1",
        "DELETE FROM groupchannels WHERE groupid = $1",
        "DELETE FROM usergroups WHERE id = $1",
    ] {
        sqlx::query(statement)
            .bind(&id)
            .execute(pool)
            .await
            .expect("the old group clears");
    }
    let stamp = |deleted: bool| if deleted { 1_700_000_001_000_i64 } else { 0 };
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid,
            createat, updateat, deleteat, allowreference)
         VALUES ($1, $2, $2, 'a group this suite made', 'custom', NULL,
                 1700000000000, 1700000000000, $3, TRUE)",
    )
    .bind(&id)
    .bind(format!("mmrs-rmgrp-{tag}"))
    .bind(stamp(group_deleted))
    .execute(pool)
    .await
    .expect("the group is written");
    sqlx::query(
        "INSERT INTO groupchannels (groupid, autoadd, schemeadmin, createat, deleteat, updateat, channelid)
         VALUES ($1, false, false, 1700000000000, $2, 1700000000000, $3)",
    )
    .bind(&id)
    .bind(stamp(link_deleted))
    .bind(channel)
    .execute(pool)
    .await
    .expect("the link is written");
    for member in members {
        sqlx::query(
            "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
             VALUES ($1, $2, 1700000000000, $3)",
        )
        .bind(&id)
        .bind(member)
        .bind(stamp(membership_deleted))
        .execute(pool)
        .await
        .expect("the membership is written");
    }
}

async fn add_user_to_team(http: &reqwest::Client, admin: &str, team_id: &str, user_id: &str) {
    let (status, _, body) = send(
        http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{team_id}/members"),
        admin,
        Some(&serde_json::json!({ "team_id": team_id, "user_id": user_id })),
    )
    .await;
    assert!(status < 300, "adding to the team: {body}");
}

/// A group-constrained channel moved with `force`: each member not on the new team is removed by
/// somebody else, so the group filter runs. A member a live group still vouches for **fails the
/// move** with `api.channel.remove_members.denied`; a deleted link, a deleted group, a deleted
/// group membership or a bot does not vouch, and the member is swept.
#[tokio::test]
async fn a_force_move_of_a_group_constrained_channel_sweeps_only_whom_no_group_vouches_for() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team_b = create_team(&http, &admin, "rmgcb").await;

    // (case, link deleted, group deleted, membership deleted, swept by a bot)
    let cases = [
        ("live", false, false, false, false),
        ("gm", false, false, true, false),
        ("gc", true, false, false, false),
        ("ug", false, true, false, false),
        ("bot", false, false, false, true),
    ];
    for (case, link_deleted, group_deleted, membership_deleted, bot) in cases {
        let mut results = Vec::new();
        for (base, side) in [(GO, "go"), (RUST, "rs")] {
            let tag = format!("rmgc{case}{side}");
            let team_a = create_team(&http, &admin, &tag).await;
            let channel = create_channel_typed(&http, &admin, &team_a, &tag, "O").await;
            let member = if bot {
                let id = common::plant_bot(&tag, logged_in_user_id(), 0)
                    .await
                    .expect("the bot");
                add_user_to_team(&http, &admin, &team_a, &id).await;
                id
            } else {
                create_plain_user(&http, &admin, &team_a, &tag).await.id
            };
            add_user_to_channel(&http, &admin, &channel, &member).await;
            plant_group(
                &pool,
                &tag,
                &channel,
                &[&member],
                link_deleted,
                group_deleted,
                membership_deleted,
            )
            .await;
            sqlx::query("UPDATE channels SET groupconstrained = true WHERE id = $1")
                .bind(&channel)
                .execute(&pool)
                .await
                .expect("the channel is group-constrained");
            invalidate_go_caches(&http, &admin).await;

            let (status, served, body) = send(
                &http,
                base,
                reqwest::Method::POST,
                &format!("/api/v4/channels/{channel}/move"),
                &admin,
                Some(&serde_json::json!({ "team_id": team_b, "force": true })),
            )
            .await;
            assert_eq!(
                served,
                base == RUST,
                "{case} {base}: served where it was sent"
            );
            let still_member = scalar(
                &pool,
                "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1 AND userid = $2",
                &[&channel, &member],
            )
            .await;
            let moved = scalar(
                &pool,
                "SELECT COUNT(*) FROM channels WHERE id = $1 AND teamid = $2",
                &[&channel, &team_b],
            )
            .await;
            results.push((status, body, still_member, moved));
            if bot {
                common::unplant_bot(&member).await;
            } else {
                common::delete_plain_user(&http, &admin, &member).await;
            }
        }
        let (rs_status, rs_body, rs_member, rs_moved) = results.pop().expect("two results");
        let (go_status, go_body, go_member, go_moved) = results.pop().expect("two results");
        let vouched = case == "live";
        assert_eq!(
            go_status,
            if vouched { 400 } else { 200 },
            "{case}: Go {go_body}"
        );
        assert_eq!(rs_status, go_status, "{case}: Rust {rs_body}");
        assert_eq!(
            (rs_member, rs_moved),
            (go_member, go_moved),
            "{case}: what the move left"
        );
        assert_eq!(go_member, i64::from(vouched), "{case}");
        if vouched {
            let go_error: serde_json::Value = serde_json::from_str(&go_body).expect("JSON");
            assert_eq!(go_error["id"], "api.channel.remove_members.denied");
            assert_error_bodies_match_except_known_gaps(
                go_body.as_bytes(),
                rs_body.as_bytes(),
                "a vouched-for member",
            );
        }
    }
}

/// Leaving a group-constrained channel **yourself** is never screened: the member a live group
/// vouches for can still take themself out.
#[tokio::test]
async fn a_member_leaves_a_group_constrained_channel_the_groups_vouch_for() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;

    let mut results = Vec::new();
    for (base, tag) in [(GO, "rmslgo"), (RUST, "rmslrs")] {
        let team = create_team(&http, &admin, tag).await;
        let channel = create_channel_typed(&http, &admin, &team, tag, "O").await;
        let member = create_plain_user(&http, &admin, &team, tag).await;
        add_user_to_channel(&http, &admin, &channel, &member.id).await;
        plant_group(&pool, tag, &channel, &[&member.id], false, false, false).await;
        sqlx::query("UPDATE channels SET groupconstrained = true WHERE id = $1")
            .bind(&channel)
            .execute(&pool)
            .await
            .expect("the channel is group-constrained");
        invalidate_go_caches(&http, &admin).await;

        let (status, served, body) = send(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}/members/{}", member.id),
            &member.token,
            None,
        )
        .await;
        assert_eq!(served, base == RUST, "{base}: served where it was sent");
        let still_member = scalar(
            &pool,
            "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1 AND userid = $2",
            &[&channel, &member.id],
        )
        .await;
        results.push((status, body, still_member));
        common::delete_plain_user(&http, &admin, &member.id).await;
    }
    let rust = results.pop().expect("two results");
    let go = results.pop().expect("two results");
    assert_eq!(go.0, 200, "Go let the member leave: {go:?}");
    assert_eq!(go.2, 0);
    assert_eq!(rust, go);
}

/// A shared channel on a server whose shared-channel service is not running is removed from like
/// any other — Go's `if scs != nil` skips `NotifyMembershipChanged` — and served here. With the
/// licence and `EnableSharedChannels` both on, the removal is handed to Go whole.
#[tokio::test]
async fn a_shared_channel_is_served_unless_the_sync_service_runs() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = logged_in_user_id();

    let mut results = Vec::new();
    for (base, tag) in [(GO, "rmshgo"), (RUST, "rmshrs")] {
        let team = create_team(&http, &admin, tag).await;
        let channel = create_channel_typed(&http, &admin, &team, tag, "O").await;
        let member = create_plain_user(&http, &admin, &team, tag).await;
        add_user_to_channel(&http, &admin, &channel, &member.id).await;
        sqlx::query("UPDATE channels SET shared = true WHERE id = $1")
            .bind(&channel)
            .execute(&pool)
            .await
            .expect("the channel is shared");
        invalidate_go_caches(&http, &admin).await;

        let mut socket = SocketProbe::connect(base, &admin).await;
        let (status, served, body) = send(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}/members/{}", member.id),
            &admin,
            None,
        )
        .await;
        assert_eq!(served, base == RUST, "{base}: served where it was sent");
        socket.collect_for(Duration::from_millis(900)).await;
        let ids = [
            (member.id.as_str(), "<member>"),
            (channel.as_str(), "<channel>"),
            (team.as_str(), "<team>"),
            (me, "<admin>"),
        ];
        let still_member = scalar(
            &pool,
            "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1 AND userid = $2",
            &[&channel, &member.id],
        )
        .await;
        let removal_posts = scalar(
            &pool,
            "SELECT COUNT(*) FROM posts WHERE channelid = $1 AND type = 'system_remove_from_channel'",
            &[&channel],
        )
        .await;
        results.push((
            status,
            body,
            still_member,
            removal_posts,
            events(&socket, &["user_removed"], &ids),
        ));
        common::delete_plain_user(&http, &admin, &member.id).await;
    }
    let rust = results.pop().expect("two results");
    let go = results.pop().expect("two results");
    assert_eq!(go.0, 200, "{go:?}");
    assert_eq!((go.2, go.3), (0, 1), "{go:?}");
    assert_eq!(rust, go);

    // The service running: the licence has shared channels, and this mm-api has the setting on.
    // Its upstream is the licensed Go, which has the setting off and so removes the member without
    // a service — but the answer is Go's, which is the assertion.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let (pair, _server) = common::licensed_rust_with(
        SHARED_RUST_PORT,
        &[(
            "MM_CONNECTEDWORKSPACESSETTINGS_ENABLESHAREDCHANNELS",
            "true",
        )],
    )
    .await;
    let team = create_team(&http, &admin, "rmshlic").await;
    let channel = create_channel_typed(&http, &admin, &team, "rmshlic", "O").await;
    let member = create_plain_user(&http, &admin, &team, "rmshlic").await;
    add_user_to_channel(&http, &admin, &channel, &member.id).await;
    sqlx::query("UPDATE channels SET shared = true WHERE id = $1")
        .bind(&channel)
        .execute(&pool)
        .await
        .expect("the channel is shared");
    common::invalidate_licensed_go_caches(&http, &pair, &admin).await;
    let (status, served, body) = send(
        &http,
        &pair.rust,
        reqwest::Method::DELETE,
        &format!("/api/v4/channels/{channel}/members/{}", member.id),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "the licensed Go removed the member: {body}");
    assert!(!served, "handed to Go while the service runs");
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1 AND userid = $2",
            &[&channel, &member.id],
        )
        .await,
        0
    );
    common::delete_plain_user(&http, &admin, &member.id).await;
}
