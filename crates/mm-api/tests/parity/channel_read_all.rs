//! Cross-server parity for the two feature-flagged "mark everything read" writes:
//! `PUT /api/v4/channels/members/{user_id}/direct/read` and
//! `PUT /api/v4/users/{user_id}/teams/{team_id}/read`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity channel_read_all
//! ```
//!
//! # The feature flag is on, on both servers, and that is a deliberate change to the stack
//!
//! `FeatureFlags.EnableShiftEscapeToMarkAllRead` defaults to `false` (feature_flags.go:181), and
//! with it off **both routes are a 501 and neither has a comparable 200**. `FeatureFlags` is
//! stripped before the configuration document is persisted, so the environment is the only place
//! either server can read it from: `scripts/go-server.sh` and `scripts/mm-api-env.sh` both set it
//! now. It is read in exactly two places in the Go tree and gates nothing else, so unlike
//! `IntegratedBoards` it cannot change an answer on a route already served.
//!
//! The **501** therefore has no cross-server oracle here: it is asserted against a
//! [`SecondServer`] started with the flag off, which is our answer only. Same shape as [D-213].
//!
//! # A write cannot be compared by posting the same request to both servers
//!
//! Each server gets its own user, as in `channel_view`. For the team route the two users are in
//! **one** team with the same memberships, so the two maps are keyed alike and the bodies are
//! comparable byte for byte. For the direct-message route they cannot be — a DM is between two
//! specific people — so the answers are compared after normalising each user's own DM id.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SecondServer, SocketProbe, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_direct_channel,
    create_plain_user, create_team, delete_plain_user, go_minted_token, post_message,
    stack_enabled,
};

fn direct_path(user: &str) -> String {
    format!("/api/v4/channels/members/{user}/direct/read")
}

fn team_path(user: &str, team: &str) -> String {
    format!("/api/v4/users/{user}/teams/{team}/read")
}

struct Fixture {
    team_id: String,
    channel_id: String,
    go_user: common::PlainUser,
    rust_user: common::PlainUser,
    go_dm: String,
    rust_dm: String,
    admin: String,
}

/// One team of its own, two users in it, one extra channel both are in with an unread message —
/// and one DM each, also unread.
///
/// The team is created rather than borrowed so that no other suite's channels are in the map this
/// route answers with.
async fn fixture(http: &reqwest::Client, tag: &str) -> Fixture {
    let admin = go_minted_token(http).await;
    common::purge_api_fixtures().await;
    let team_id = create_team(http, &admin, tag).await;
    let channel_id = create_channel(http, &admin, &team_id, tag).await;

    let go_user = create_plain_user(http, &admin, &team_id, &format!("{tag}g")).await;
    let rust_user = create_plain_user(http, &admin, &team_id, &format!("{tag}r")).await;
    add_user_to_channel(http, &admin, &channel_id, &go_user.id).await;
    add_user_to_channel(http, &admin, &channel_id, &rust_user.id).await;

    let go_dm = create_direct_channel(http, &admin, common::logged_in_user_id(), &go_user.id).await;
    let rust_dm =
        create_direct_channel(http, &admin, common::logged_in_user_id(), &rust_user.id).await;

    // Every post is the admin's, so both users' counters move and neither user's own does.
    post_message(http, &admin, &channel_id, "mmrs read-all fixture", None).await;
    post_message(http, &admin, &go_dm, "mmrs dm for go", None).await;
    post_message(http, &admin, &rust_dm, "mmrs dm for rust", None).await;

    Fixture {
        team_id,
        channel_id,
        go_user,
        rust_user,
        go_dm,
        rust_dm,
        admin,
    }
}

async fn cleanup(http: &reqwest::Client, f: &Fixture) {
    delete_plain_user(http, &f.admin, &f.go_user.id).await;
    delete_plain_user(http, &f.admin, &f.rust_user.id).await;
}

async fn put_one(http: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, Vec<u8>) {
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}

/// `PUT /users/{user_id}/teams/{team_id}/read` — byte-identical for two users with the same
/// memberships in the same team.
///
/// The map carries **every** channel in the team the user belongs to, including the default ones
/// and the ones that were already read: `times` is built from the whole membership set, not from
/// the unread part of it.
#[tokio::test]
async fn the_team_answer_is_byte_identical_and_covers_every_membership() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readteam").await;

    let (go_status, go_body) = put_one(
        &http,
        GO,
        &f.go_user.token,
        &team_path(&f.go_user.id, &f.team_id),
    )
    .await;
    let (rs_status, rs_body) = put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path(&f.rust_user.id, &f.team_id),
    )
    .await;

    cleanup(&http, &f).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two answers must be byte-identical, newline included"
    );

    let answered: serde_json::Value =
        serde_json::from_slice(&go_body).expect("a ChannelViewResponse");
    let times = answered["last_viewed_at_times"]
        .as_object()
        .expect("a map, not a null");
    assert!(
        times.contains_key(&f.channel_id),
        "the fixture channel is in it"
    );
    assert!(
        times.len() >= 3,
        "town-square and off-topic are in it too: {times:?}"
    );
}

/// The team route leaves the team read, on both servers, and leaves the user's **DMs alone** —
/// a DM has no `TeamId`, so it is not in the team query at all.
#[tokio::test]
async fn the_team_write_lands_and_does_not_touch_the_direct_messages() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readteamw").await;

    let unread = async |token: &str, channel: &str| -> i64 {
        let value: serde_json::Value = http
            .get(format!("{GO}/api/v4/users/me/channels/{channel}/unread"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("a ChannelUnread");
        value["msg_count"].as_i64().unwrap_or(-1)
    };

    assert_eq!(unread(&f.go_user.token, &f.channel_id).await, 1);
    assert_eq!(unread(&f.rust_user.token, &f.channel_id).await, 1);

    put_one(&http, GO, &f.go_user.token, &team_path("me", &f.team_id)).await;
    put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path("me", &f.team_id),
    )
    .await;

    let go_channel = unread(&f.go_user.token, &f.channel_id).await;
    let rust_channel = unread(&f.rust_user.token, &f.channel_id).await;
    let go_dm = unread(&f.go_user.token, &f.go_dm).await;
    let rust_dm = unread(&f.rust_user.token, &f.rust_dm).await;

    cleanup(&http, &f).await;

    assert_eq!(go_channel, 0, "Go marked the team channel read");
    assert_eq!(rust_channel, go_channel, "and so did we");
    assert_eq!(go_dm, 1, "Go left the DM unread — it has no team");
    assert_eq!(rust_dm, go_dm, "and so did we");
}

/// `PUT /channels/members/{user_id}/direct/read` — the mirror image: the DM is marked read and
/// the team channel is not.
///
/// The two users' maps are keyed by *their own* DM ids, so the bodies are compared after
/// substituting a placeholder for each.
#[tokio::test]
async fn the_direct_answer_matches_once_each_users_own_dm_id_is_normalised() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readdm").await;

    let (go_status, go_body) =
        put_one(&http, GO, &f.go_user.token, &direct_path(&f.go_user.id)).await;
    let (rs_status, rs_body) = put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &direct_path(&f.rust_user.id),
    )
    .await;

    let unread = async |token: &str, channel: &str| -> i64 {
        let value: serde_json::Value = http
            .get(format!("{GO}/api/v4/users/me/channels/{channel}/unread"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("a ChannelUnread");
        value["msg_count"].as_i64().unwrap_or(-1)
    };
    let go_dm = unread(&f.go_user.token, &f.go_dm).await;
    let rust_dm = unread(&f.rust_user.token, &f.rust_dm).await;
    let go_channel = unread(&f.go_user.token, &f.channel_id).await;
    let rust_channel = unread(&f.rust_user.token, &f.channel_id).await;

    let last_post_at = async |channel: &str| -> i64 {
        let value: serde_json::Value = http
            .get(format!("{GO}/api/v4/channels/{channel}"))
            .header("Authorization", format!("Bearer {}", f.admin))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("a Channel");
        value["last_post_at"].as_i64().unwrap_or(-1)
    };
    let go_dm_last_post_at = last_post_at(&f.go_dm).await;
    let rust_dm_last_post_at = last_post_at(&f.rust_dm).await;

    cleanup(&http, &f).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);

    // **The id and the timestamp are both per-DM.** The two channels were opened a few
    // milliseconds apart, so their `LastPostAt`s differ by that much; normalising only the id
    // would compare two clocks. What is left after normalising both is the shape, which is the
    // whole comparable part of a per-user answer — and the values are then checked against the
    // channel each server actually answered about.
    let normalised = |body: &[u8], dm: &str, at: i64| -> String {
        String::from_utf8_lossy(body)
            .replace(dm, "<dm>")
            .replace(&at.to_string(), "<last_post_at>")
    };
    assert_eq!(
        normalised(&rs_body, &f.rust_dm, rust_dm_last_post_at),
        normalised(&go_body, &f.go_dm, go_dm_last_post_at),
        "same shape, one key each"
    );

    let value_of = |body: &[u8], dm: &str| -> i64 {
        let answered: serde_json::Value =
            serde_json::from_slice(body).expect("a ChannelViewResponse");
        let times = answered["last_viewed_at_times"]
            .as_object()
            .expect("a map, not a null")
            .clone();
        assert_eq!(times.len(), 1, "a fresh user has exactly one DM: {times:?}");
        times[dm].as_i64().expect("a timestamp")
    };
    assert_eq!(
        value_of(&go_body, &f.go_dm),
        go_dm_last_post_at,
        "the value is the channel's own LastPostAt, not a clock reading"
    );
    assert_eq!(
        value_of(&rs_body, &f.rust_dm),
        rust_dm_last_post_at,
        "and ours is too"
    );

    assert_eq!(go_dm, 0, "Go marked the DM read");
    assert_eq!(rust_dm, go_dm, "and so did we");
    assert_eq!(go_channel, 1, "Go left the team channel unread");
    assert_eq!(rust_channel, go_channel, "and so did we");
}

/// **The team route's thread event is one, team-scoped**; the direct route's is one *per channel*
/// with no team. Both are gated on CRT being on for the user, which the stack's `always_on`
/// default makes unconditional.
#[tokio::test]
async fn the_two_routes_scope_their_thread_events_differently() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readsock").await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    put_one(&http, GO, &f.go_user.token, &team_path("me", &f.team_id)).await;
    put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path("me", &f.team_id),
    )
    .await;
    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;

    let go_threads = go_socket.events_named("thread_read_changed");
    let rust_threads = rust_socket.events_named("thread_read_changed");
    assert_eq!(go_threads.len(), 1, "Go: {:?}", go_socket.raw);
    assert_eq!(
        rust_threads.len(),
        go_threads.len(),
        "{:?}",
        rust_socket.raw
    );
    assert_eq!(
        go_threads[0]["broadcast"]["team_id"],
        serde_json::json!(f.team_id),
        "team-scoped"
    );
    assert_eq!(
        rust_threads[0]["broadcast"]["team_id"],
        go_threads[0]["broadcast"]["team_id"]
    );
    assert_eq!(
        rust_threads[0]["broadcast"]["channel_id"], go_threads[0]["broadcast"]["channel_id"],
        "and not channel-scoped"
    );

    // Now the direct route, on the same sockets.
    go_socket.raw.clear();
    rust_socket.raw.clear();
    put_one(&http, GO, &f.go_user.token, &direct_path("me")).await;
    put_one(&http, RUST, &f.rust_user.token, &direct_path("me")).await;
    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;

    let go_dm_threads = go_socket.events_named("thread_read_changed");
    let rust_dm_threads = rust_socket.events_named("thread_read_changed");

    cleanup(&http, &f).await;

    assert_eq!(
        go_dm_threads.len(),
        1,
        "one per DM, and the user has one: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_dm_threads.len(),
        go_dm_threads.len(),
        "{:?}",
        rust_socket.raw
    );
    assert_eq!(
        go_dm_threads[0]["broadcast"]["team_id"],
        serde_json::json!(""),
        "no team to broadcast on"
    );
    assert_eq!(
        rust_dm_threads[0]["broadcast"]["team_id"],
        go_dm_threads[0]["broadcast"]["team_id"]
    );
    // Channel-scoped instead — and each server scoped it to *its own* user's DM, so the two ids
    // are supposed to differ.
    assert_eq!(
        go_dm_threads[0]["broadcast"]["channel_id"],
        serde_json::json!(f.go_dm)
    );
    assert_eq!(
        rust_dm_threads[0]["broadcast"]["channel_id"],
        serde_json::json!(f.rust_dm)
    );
}

/// A second press changes nothing and publishes no `multiple_channels_viewed` — but **still
/// publishes the thread event**, because that one is not behind the early return.
#[tokio::test]
async fn a_second_press_is_silent_except_for_the_thread_event() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readtwice").await;

    put_one(&http, GO, &f.go_user.token, &team_path("me", &f.team_id)).await;
    put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path("me", &f.team_id),
    )
    .await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    let (go_status, go_body) =
        put_one(&http, GO, &f.go_user.token, &team_path("me", &f.team_id)).await;
    let (rs_status, rs_body) = put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path("me", &f.team_id),
    )
    .await;
    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;

    let go_viewed = go_socket.events_named("multiple_channels_viewed").len();
    let rust_viewed = rust_socket.events_named("multiple_channels_viewed").len();
    let go_threads = go_socket.events_named("thread_read_changed").len();
    let rust_threads = rust_socket.events_named("thread_read_changed").len();

    cleanup(&http, &f).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the answer repeats"
    );
    assert_eq!(go_viewed, 0, "nothing was unread: {:?}", go_socket.raw);
    assert_eq!(rust_viewed, go_viewed, "{:?}", rust_socket.raw);
    assert_eq!(
        go_threads, 1,
        "the thread event is not behind the early return: {:?}",
        go_socket.raw
    );
    assert_eq!(rust_threads, go_threads, "{:?}", rust_socket.raw);
}

/// The refusals, which write nothing and can be driven through both servers with one user.
#[tokio::test]
async fn the_refusals_match_on_both_routes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team_id, "readrefuse").await;
    let victim = create_plain_user(&http, &admin, &team_id, "readvictim").await;

    let put_both = async |token: &str, path: &str| -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
        let go = http
            .put(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let go = (
            go.status().as_u16(),
            go.bytes().await.expect("body").to_vec(),
        );
        let rs = http
            .put(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        let status = rs.status().as_u16();
        common::assert_served_by_rust(rs.headers(), path);
        (go, (status, rs.bytes().await.expect("body").to_vec()))
    };

    // A malformed user id: both routes return early here, unlike `mark_read`.
    for path in [direct_path("short"), team_path("short", &team_id)] {
        let ((go_status, go_body), (rs_status, rs_body)) = put_both(&plain.token, &path).await;
        assert_eq!(
            go_status,
            400,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }

    // A malformed team id, which is checked second.
    let path = team_path(&plain.id, "short");
    let ((go_status, go_body), (rs_status, rs_body)) = put_both(&plain.token, &path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    // Another user, with no `edit_other_users`.
    for path in [direct_path(&victim.id), team_path(&victim.id, &team_id)] {
        let ((go_status, go_body), (rs_status, rs_body)) = put_both(&plain.token, &path).await;
        assert_eq!(
            go_status,
            403,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.permissions.app_error");
    }

    // **A team the caller is not in is the `view_team` refusal, not a 404** — there is no
    // `GetTeamMember` on this path.
    let other_team = create_team(&http, &admin, "readotherteam").await;
    let path = team_path(&plain.id, &other_team);
    let ((go_status, go_body), (rs_status, rs_body)) = put_both(&plain.token, &path).await;

    delete_plain_user(&http, &admin, &plain.id).await;
    delete_plain_user(&http, &admin, &victim.id).await;

    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "other team");
}

/// No session is a 401 on both, and it comes **after** the feature-flag gate — so with the flag
/// off these routes would answer 501 to an anonymous caller.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    for path in [
        direct_path("me"),
        team_path("me", "nosuchteam0000000000000000"),
    ] {
        let go = http
            .put(format!("{GO}{path}"))
            .send()
            .await
            .expect("Go answers");
        let go = (
            go.status().as_u16(),
            go.bytes().await.expect("body").to_vec(),
        );
        let rs = http
            .put(format!("{RUST}{path}"))
            .send()
            .await
            .expect("we answer");
        let rs = (
            rs.status().as_u16(),
            rs.bytes().await.expect("body").to_vec(),
        );
        assert_eq!(go.0, 401, "{path}");
        assert_eq!(rs.0, go.0, "{path}");
        assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
    }
}

/// **With the flag off both routes are a 501, and the gate is the first line of each handler** —
/// ahead of `RequireUserId`, so a malformed id gets the 501 too.
///
/// Our answer only: the stack's Go server runs with the flag on, and a second Go on a different
/// environment is what [D-213] already owes.
#[tokio::test]
async fn with_the_flag_off_both_routes_are_a_501() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let Some(server) = SecondServer::start(
        8074,
        &[("MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD", "false")],
    )
    .await
    else {
        eprintln!("skipping: target/debug/mm-api is not built, so no second server");
        return;
    };

    for path in [
        direct_path("me"),
        direct_path("short"),
        team_path("me", "nosuchteam0000000000000000"),
        team_path("short", "short"),
    ] {
        let response = http
            .put(format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the second server answers");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("an error body");
        assert_eq!(
            status, 501,
            "{path} must be refused before the id is validated: {body}"
        );
        assert_eq!(
            body["id"], "api.mark_all_as_read.disabled.app_error",
            "{path}"
        );
    }
}

/// **The team route hands the thread store *every* channel, not the unread ones** — and this is
/// the only test that can tell the two apart.
///
/// A thread reply does not bump `Channels.TotalMsgCount`, so a channel whose counters are already
/// caught up can still hold unread thread replies. Go passes the full membership set for exactly
/// that reason (its comment is at app/channel.go:3566), and the thread store's own
/// `LastReplyAt > LastViewed` clause keeps the write bounded.
///
/// The fixture forces the case: the user starts a thread, the admin replies to it — which makes
/// the user a follower — and then the user **views the channel** saying it supports collapsed
/// threads, which marks the channel read and deliberately leaves the thread membership alone.
/// At that point `with_unreads` is empty and the thread is not. Passing only `with_unreads` would
/// leave `total_unread_threads` at 1.
#[tokio::test]
async fn the_team_route_marks_threads_read_in_channels_that_were_already_read() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "readthread").await;

    let unread_threads = async |token: &str, user: &str| -> i64 {
        let value: serde_json::Value = http
            .get(format!(
                "{GO}/api/v4/users/{user}/teams/{}/threads?unread=true&totalsOnly=true",
                f.team_id
            ))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("a ThreadsResponse");
        value["total_unread_threads"].as_i64().unwrap_or(-1)
    };

    for user in [&f.go_user, &f.rust_user] {
        let root = post_message(&http, &user.token, &f.channel_id, "mmrs thread root", None).await;
        post_message(
            &http,
            &f.admin,
            &f.channel_id,
            "mmrs thread reply",
            Some(&root),
        )
        .await;
        // Says it renders threads itself, so the channel is marked read and the thread membership
        // is not — which is the state this route has to handle.
        http.post(format!("{GO}/api/v4/channels/members/me/view"))
            .header("Authorization", format!("Bearer {}", user.token))
            .json(&serde_json::json!({
                "channel_id": f.channel_id,
                "collapsed_threads_supported": true,
            }))
            .send()
            .await
            .expect("Go answers");
    }

    assert_eq!(
        unread_threads(&f.go_user.token, "me").await,
        1,
        "the thread is unread before the press"
    );
    assert_eq!(unread_threads(&f.rust_user.token, "me").await, 1);

    put_one(&http, GO, &f.go_user.token, &team_path("me", &f.team_id)).await;
    put_one(
        &http,
        RUST,
        &f.rust_user.token,
        &team_path("me", &f.team_id),
    )
    .await;

    let go_after = unread_threads(&f.go_user.token, "me").await;
    let rust_after = unread_threads(&f.rust_user.token, "me").await;

    cleanup(&http, &f).await;

    assert_eq!(go_after, 0, "Go marked the thread read");
    assert_eq!(rust_after, go_after, "and so did we");
}
