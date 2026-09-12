//! Cross-server parity for the two mark-a-channel-read writes:
//! `POST /api/v4/channels/members/{user_id}/view` and
//! `POST /api/v4/channels/members/{user_id}/mark_read`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity channel_view
//! ```
//!
//! # A write cannot be compared by posting the same request to both servers
//!
//! The first POST marks the channel read, so the second sees a different row and answers a
//! different `last_viewed_at_times`. Every test here that reaches the write therefore gives each
//! server **its own user**, both members of the same channel with the same unread state, exactly
//! as `status_writes` does — so the two answers are comparable because the inputs are, not
//! because the servers were lucky about ordering. The refusal tests write nothing and can drive
//! one user through both.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, add_user_to_channel, assert_error_bodies_match_except_known_gaps,
    client, create_channel, create_plain_user, delete_plain_user, go_minted_token, post_both_raw,
    post_message, stack_enabled,
};

fn view_path(user: &str) -> String {
    format!("/api/v4/channels/members/{user}/view")
}

fn mark_read_path(user: &str) -> String {
    format!("/api/v4/channels/members/{user}/mark_read")
}

/// One channel, two fresh members, and a message from the admin that neither has seen.
struct Fixture {
    channel_id: String,
    go_user: common::PlainUser,
    rust_user: common::PlainUser,
    admin: String,
    team_id: String,
}

async fn fixture(http: &reqwest::Client, tag: &str) -> Fixture {
    let admin = go_minted_token(http).await;
    common::purge_api_fixtures().await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(http, &admin).await;
    let channel_id = create_channel(http, &admin, &team_id, tag).await;

    let go_user = create_plain_user(http, &admin, &team_id, &format!("{tag}g")).await;
    let rust_user = create_plain_user(http, &admin, &team_id, &format!("{tag}r")).await;
    add_user_to_channel(http, &admin, &channel_id, &go_user.id).await;
    add_user_to_channel(http, &admin, &channel_id, &rust_user.id).await;

    // Somebody else has to speak, or `TotalMsgCount - MsgCount` is zero and the whole write is
    // the early-return path.
    post_message(http, &admin, &channel_id, "mmrs view fixture", None).await;

    Fixture {
        channel_id,
        go_user,
        rust_user,
        admin,
        team_id,
    }
}

async fn cleanup(http: &reqwest::Client, f: &Fixture) {
    delete_plain_user(http, &f.admin, &f.go_user.id).await;
    delete_plain_user(http, &f.admin, &f.rust_user.id).await;
}

/// Post as `user`, to `base` only.
async fn post_one(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, Vec<u8>) {
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}

/// The whole point of the route: two equivalent users, one channel, byte-identical answers.
///
/// The body is `{"status":"OK","last_viewed_at_times":{<channel>:<last_post_at>}}` with a
/// trailing newline — `json.NewEncoder` framing ([D-086]) — and the timestamp is the *channel's*
/// `LastPostAt`, shared by both users, which is what makes the two bodies comparable at all.
#[tokio::test]
async fn the_view_answer_is_byte_identical_for_two_equivalent_users() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "vieweq").await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    let (go_status, go_body) = post_one(
        &http,
        GO,
        &f.go_user.token,
        &view_path(&f.go_user.id),
        &body,
    )
    .await;
    let (rs_status, rs_body) = post_one(
        &http,
        RUST,
        &f.rust_user.token,
        &view_path(&f.rust_user.id),
        &body,
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
    assert!(
        go_body.ends_with(b"}}\n"),
        "encoder-framed: {:?}",
        String::from_utf8_lossy(&go_body)
    );
}

/// **Viewing again is a 200 with the same body and no second write.** The unread set is empty the
/// second time, so `MarkChannelsAsViewed` returns before it touches anything — and `read_times`
/// still carries the channel, because it is built from every membership rather than from the
/// unread ones.
#[tokio::test]
async fn a_second_view_answers_the_same_and_writes_nothing() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewtwice").await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    let (_, go_first) = post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    let (_, go_second) = post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    let (_, rs_first) = post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;
    let (_, rs_second) = post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    cleanup(&http, &f).await;

    assert_eq!(go_second, go_first, "Go's second answer repeats the first");
    assert_eq!(rs_second, rs_first, "and so does ours");
    assert_eq!(rs_first, go_first);
}

/// The mark-read route over the same fixture, with the same comparison.
///
/// Its body is a **list of channel ids**, not a `ChannelView`, and it passes
/// `collapsedThreadsSupported: true` unconditionally — so with CRT on it never touches thread
/// memberships, where `view` would for a client that said it does not render threads.
#[tokio::test]
async fn mark_read_answers_the_same_shape_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "markread").await;

    let body = serde_json::json!([f.channel_id]);
    let (go_status, go_body) = post_one(
        &http,
        GO,
        &f.go_user.token,
        &mark_read_path(&f.go_user.id),
        &body,
    )
    .await;
    let (rs_status, rs_body) = post_one(
        &http,
        RUST,
        &f.rust_user.token,
        &mark_read_path(&f.rust_user.id),
        &body,
    )
    .await;

    cleanup(&http, &f).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
}

/// The read state a client can actually see afterwards: `GET /users/me/channels/{id}/unread` must
/// agree on both servers once each has marked its own user's channel read.
///
/// This is the assertion the response body cannot make. `last_viewed_at_times` is computed
/// *before* the UPDATE, so a port that answered correctly and wrote nothing at all would pass
/// every test above.
#[tokio::test]
async fn the_write_actually_lands_and_both_servers_agree_on_the_result() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewwrite").await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    let unread_before: serde_json::Value = http
        .get(format!(
            "{GO}/api/v4/users/me/channels/{}/unread",
            f.channel_id
        ))
        .header("Authorization", format!("Bearer {}", f.go_user.token))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a ChannelUnread");
    assert_eq!(
        unread_before["msg_count"], 1,
        "the fixture message is unread before the view"
    );

    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    let read_back = async |token: &str| -> serde_json::Value {
        http.get(format!(
            "{GO}/api/v4/users/me/channels/{}/unread",
            f.channel_id
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a ChannelUnread")
    };
    // Both read through **Go**, so the comparison is of two rows written by two servers rather
    // than of two servers' opinions about one row.
    let after_go = read_back(&f.go_user.token).await;
    let after_rust = read_back(&f.rust_user.token).await;

    cleanup(&http, &f).await;

    assert_eq!(after_go["msg_count"], 0, "Go marked its user read");
    assert_eq!(
        after_rust["msg_count"], after_go["msg_count"],
        "and so did we: {after_rust}"
    );
    assert_eq!(after_rust["mention_count"], after_go["mention_count"]);
    assert_eq!(after_rust["msg_count_root"], after_go["msg_count_root"]);
}

/// **A blank body is the "focus loss" request**: no ids, an empty map, and a 200. Nothing is
/// written, so one user through both servers is safe.
#[tokio::test]
async fn an_empty_view_is_an_empty_map_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    for raw in [&b"{}"[..], &b"null"[..], &br#"{"channel_id":""}"#[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, &view_path("me"), raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 200, "[{shown}] must be accepted by Go");
        assert_eq!(rs_status, go_status, "[{shown}]");
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "[{shown}]"
        );
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            "{\"status\":\"OK\",\"last_viewed_at_times\":{}}\n",
            "[{shown}]: an empty map, not a null"
        );
    }
}

/// A body that will not decode, and one that decodes to the wrong *kind* of value.
///
/// `[]` is the trap: Go refuses an array into a struct, while serde's derive would build one
/// positionally — so a port that deserialised straight into `ChannelView` answers 200 here.
#[tokio::test]
async fn a_bad_view_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    for raw in [
        &b"not json"[..],
        &b"[]"[..],
        &br#"["x","y",true]"#[..],
        &b"7"[..],
        &b""[..],
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, &view_path("me"), raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}] must be rejected by Go");
        assert_eq!(rs_status, go_status, "[{shown}]");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
    }
}

/// A malformed id **inside** the body — for each of the two fields, since they are separate
/// checks in Go and `channel_id` is tested first.
#[tokio::test]
async fn a_malformed_id_in_the_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    for raw in [
        &br#"{"channel_id":"nope"}"#[..],
        &br#"{"prev_channel_id":"nope"}"#[..],
        &br#"{"channel_id":"nope","prev_channel_id":"alsonope"}"#[..],
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, &view_path("me"), raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}]");
        assert_eq!(rs_status, go_status, "[{shown}]");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
    }
}

/// A well-formed id naming **no channel** is a 200, not a 404 — the join simply matches nothing
/// and the id is absent from the answer.
#[tokio::test]
async fn an_unknown_but_well_formed_channel_id_is_an_empty_map() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    let raw = br#"{"channel_id":"mmrsnosuchchannel000000000"}"#;
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &token, &view_path("me"), raw).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        "{\"status\":\"OK\",\"last_viewed_at_times\":{}}\n"
    );
}

/// **A board id is a 400 with its own error id**, not a 404 and not a silent 200 — which is the
/// only reason `GetBoardChannel` is ported at all.
#[tokio::test]
async fn a_board_channel_is_refused_by_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &token).await;

    let Some(board) = common::plant_channel_of_type(&team_id, "BO", "viewboard").await else {
        eprintln!("skipping: DATABASE_URL is unset, so no board can be planted");
        return;
    };

    for field in ["channel_id", "prev_channel_id"] {
        let raw = serde_json::json!({ field: board }).to_string();
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, &view_path("me"), raw.as_bytes()).await;
        assert_eq!(
            go_status,
            400,
            "{field}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, go_status, "{field}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, field);
        assert_eq!(go["id"], "api.channel.board_channel.app_error");
    }
}

/// A malformed `user_id` in the **path** is a 400 on `view` — where `RequireUserId` returns
/// early — and a **403** on `mark_read`, where it does not and the permission check overwrites
/// it. Same two lines of Go, one `if` apart.
#[tokio::test]
async fn a_malformed_path_user_id_answers_differently_on_the_two_routes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    // A *plain* user: an admin holds `edit_other_users`, which would take the mark-read case down
    // the app-error path instead of the 403.
    let plain = create_plain_user(&http, &admin, &team_id, "viewbadid").await;

    // Alphanumeric on purpose: a hyphen is outside gorilla's `{user_id:[A-Za-z0-9]+}` charset,
    // so both routers would 404 the path itself and the proxy would forward it — testing the
    // charset guard rather than `RequireUserId`.
    let bad = "short";
    let ((go_view, go_view_body), (rs_view, rs_view_body)) =
        post_both_raw(&http, &plain.token, &view_path(bad), b"{}").await;
    assert_eq!(go_view, 400, "view returns early on RequireUserId");
    assert_eq!(rs_view, go_view);
    let go = assert_error_bodies_match_except_known_gaps(&go_view_body, &rs_view_body, "view");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    let ((go_mark, go_mark_body), (rs_mark, rs_mark_body)) =
        post_both_raw(&http, &plain.token, &mark_read_path(bad), br#"["x"]"#).await;
    delete_plain_user(&http, &admin, &plain.id).await;

    assert_eq!(
        go_mark,
        403,
        "mark_read does not return, so the permission refusal overwrites the 400: {}",
        String::from_utf8_lossy(&go_mark_body)
    );
    assert_eq!(rs_mark, go_mark);
    assert_error_bodies_match_except_known_gaps(&go_mark_body, &rs_mark_body, "mark_read");
}

/// `mark_read`'s body checks, which are `SortedArrayFromJSON`'s and not `ChannelView`'s: an
/// unparseable body is `payload.parse` and an **empty or null** list is `invalid_body_param`.
#[tokio::test]
async fn a_bad_mark_read_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    for (raw, id) in [
        (&b"not json"[..], "api.payload.parse.error"),
        (&b"{}"[..], "api.payload.parse.error"),
        (&b""[..], "api.payload.parse.error"),
        (&b"[]"[..], "api.context.invalid_body_param.app_error"),
        (&b"null"[..], "api.context.invalid_body_param.app_error"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, &mark_read_path("me"), raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(
            go_status,
            400,
            "[{shown}]: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, go_status, "[{shown}]");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], id, "[{shown}]");
    }
}

/// Another user's id, with no `edit_other_users`: 403 naming that permission, on both routes.
#[tokio::test]
async fn viewing_for_another_user_is_a_403_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team_id, channel_id) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team_id, "viewother").await;
    let victim = create_plain_user(&http, &admin, &team_id, "viewvictim").await;

    let body = serde_json::json!({ "channel_id": channel_id }).to_string();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &plain.token, &view_path(&victim.id), body.as_bytes()).await;
    let list = serde_json::json!([channel_id]).to_string();
    let ((go_mark, go_mark_body), (rs_mark, rs_mark_body)) = post_both_raw(
        &http,
        &plain.token,
        &mark_read_path(&victim.id),
        list.as_bytes(),
    )
    .await;

    delete_plain_user(&http, &admin, &plain.id).await;
    delete_plain_user(&http, &admin, &victim.id).await;

    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "view");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    assert_eq!(go_mark, 403, "{}", String::from_utf8_lossy(&go_mark_body));
    assert_eq!(rs_mark, go_mark);
    assert_error_bodies_match_except_known_gaps(&go_mark_body, &rs_mark_body, "mark_read");
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, "", &view_path("me"), b"{}").await;
    assert_eq!(go_status, 401);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no session");
}

/// **The websocket event both servers publish**: one `multiple_channels_viewed`, addressed to the
/// user, carrying `channel_times` — the whole read-time map, not only the channels that moved.
///
/// Two probes on two servers with two users, for the reason the module docs give; the frames are
/// compared field by field rather than byte for byte, because `seq` counts each socket's own
/// broadcast history.
#[tokio::test]
async fn both_servers_publish_one_multiple_channels_viewed() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewsock").await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    let seen = |probe: &SocketProbe| !probe.events_named("multiple_channels_viewed").is_empty();
    go_socket
        .collect_until(Duration::from_secs(3), |_| false)
        .await;
    rust_socket
        .collect_until(Duration::from_secs(3), |_| false)
        .await;

    let go_events = go_socket.events_named("multiple_channels_viewed");
    let rust_events = rust_socket.events_named("multiple_channels_viewed");

    cleanup(&http, &f).await;

    assert!(
        seen(&go_socket),
        "Go published nothing: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        go_events.len(),
        "one event each: go={go_events:?} rust={rust_events:?}"
    );

    let go_event = &go_events[0];
    let rust_event = &rust_events[0];
    assert_eq!(rust_event["event"], go_event["event"]);
    // Each probe is a different user's socket, so the two ids are *supposed* to differ — what
    // must match is that each server addressed its own actor and nobody else.
    assert_eq!(
        go_event["broadcast"]["user_id"],
        serde_json::json!(f.go_user.id),
        "Go addressed its own actor"
    );
    assert_eq!(
        rust_event["broadcast"]["user_id"],
        serde_json::json!(f.rust_user.id),
        "and so did we"
    );
    assert_eq!(
        rust_event["broadcast"]["channel_id"], go_event["broadcast"]["channel_id"],
        "not channel-scoped on either"
    );
    assert_eq!(
        rust_event["data"]["channel_times"][&f.channel_id],
        go_event["data"]["channel_times"][&f.channel_id],
        "the same read time for the same channel"
    );
    let _ = &f.team_id;
}

/// **`collapsed_threads_supported` decides whether the thread half of the write happens**, and
/// the only place a client can see it is the socket.
///
/// `updateThreads` is `ThreadAutoFollow && (!collapsedThreadsSupported || !isCRTEnabled)`. The
/// stack's `CollapsedThreads` is the shipped `always_on`, so `isCRTEnabled` is true for everyone
/// and the expression is exactly `!collapsedThreadsSupported`. A client that says it renders
/// threads itself therefore gets **no** `thread_read_changed`; one that says nothing gets one per
/// channel marked read.
#[tokio::test]
async fn the_client_flag_decides_whether_thread_read_changed_is_published() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewcrt").await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    // Says it supports collapsed threads: no thread write, no thread event.
    let supported = serde_json::json!({
        "channel_id": f.channel_id,
        "collapsed_threads_supported": true,
    });
    post_one(&http, GO, &f.go_user.token, &view_path("me"), &supported).await;
    post_one(
        &http,
        RUST,
        &f.rust_user.token,
        &view_path("me"),
        &supported,
    )
    .await;
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_threads = go_socket.events_named("thread_read_changed").len();
    let rust_threads = rust_socket.events_named("thread_read_changed").len();
    assert_eq!(
        go_threads, 0,
        "Go published one anyway: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_threads, go_threads,
        "we published one where Go did not: {:?}",
        rust_socket.raw
    );

    cleanup(&http, &f).await;
}

/// The other half of the same flag, on a channel that is still unread.
///
/// A client that omits `collapsed_threads_supported` gets `thread_read_changed` — one per channel
/// in the unread set, with a `channel_id` broadcast scope and no team.
#[tokio::test]
async fn omitting_the_client_flag_publishes_thread_read_changed_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewnocrt").await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    let waited = async |probe: &mut SocketProbe| {
        probe
            .collect_until(Duration::from_secs(3), |frames| {
                frames
                    .iter()
                    .any(|f| f.get("event").and_then(|e| e.as_str()) == Some("thread_read_changed"))
            })
            .await
    };
    let go_seen = waited(&mut go_socket).await;
    let rust_seen = waited(&mut rust_socket).await;

    let go_events = go_socket.events_named("thread_read_changed");
    let rust_events = rust_socket.events_named("thread_read_changed");

    cleanup(&http, &f).await;

    assert!(go_seen, "Go published none: {:?}", go_socket.raw);
    assert!(rust_seen, "we published none: {:?}", rust_socket.raw);
    assert_eq!(
        rust_events.len(),
        go_events.len(),
        "one per channel marked read on both"
    );
    assert_eq!(
        rust_events[0]["broadcast"]["channel_id"], go_events[0]["broadcast"]["channel_id"],
        "channel-scoped on both"
    );
    assert_eq!(
        go_events[0]["broadcast"]["channel_id"],
        serde_json::json!(f.channel_id)
    );
}

/// **`mark_read` passes `collapsedThreadsSupported: true` unconditionally**, so it never publishes
/// a thread event where `view` with the same body would.
#[tokio::test]
async fn mark_read_never_publishes_a_thread_event() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "markreadcrt").await;

    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    let body = serde_json::json!([f.channel_id]);
    post_one(&http, GO, &f.go_user.token, &mark_read_path("me"), &body).await;
    post_one(
        &http,
        RUST,
        &f.rust_user.token,
        &mark_read_path("me"),
        &body,
    )
    .await;
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_threads = go_socket.events_named("thread_read_changed").len();
    let rust_threads = rust_socket.events_named("thread_read_changed").len();
    let go_viewed = go_socket.events_named("multiple_channels_viewed").len();
    let rust_viewed = rust_socket.events_named("multiple_channels_viewed").len();

    cleanup(&http, &f).await;

    assert_eq!(go_threads, 0, "Go published one: {:?}", go_socket.raw);
    assert_eq!(rust_threads, go_threads, "{:?}", rust_socket.raw);
    assert_eq!(go_viewed, 1, "but the viewed event is published");
    assert_eq!(rust_viewed, go_viewed, "{:?}", rust_socket.raw);
}

/// **A view that changes nothing publishes nothing.** The early return in `MarkChannelsAsViewed`
/// is before the events, not after them, so re-viewing an already-read channel is silent — which
/// is the only observable difference between "returned early" and "wrote the same values again".
#[tokio::test]
async fn a_second_view_publishes_no_second_event() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewsilent").await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    // Connect *after* the first view, so only the second one's events can arrive.
    let mut go_socket = SocketProbe::connect(GO, &f.go_user.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust_user.token).await;

    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_viewed = go_socket.events_named("multiple_channels_viewed").len();
    let rust_viewed = rust_socket.events_named("multiple_channels_viewed").len();

    cleanup(&http, &f).await;

    assert_eq!(go_viewed, 0, "Go was not silent: {:?}", go_socket.raw);
    assert_eq!(rust_viewed, go_viewed, "we were not: {:?}", rust_socket.raw);
}

/// **`SetActiveChannel` runs on every view and writes no row**, on either server.
///
/// Go puts the status in `platform.statusCache` and broadcasts only when the status *string*
/// changed; nothing reaches the `Status` table. A port that reached for `SaveAndBroadcastStatus`
/// here — the obvious-looking neighbour — would leave a row where Go leaves none, and no response
/// body would show it.
///
/// **What this cannot assert is the cache itself.** `mm-app`'s `get_user_statuses_by_ids` reads
/// the table only (see its doc comment, which names `SetActiveChannel` as the divergence), so the
/// cache entry this route writes is not readable back through any route we serve. The mutation
/// that deletes the `set_active_channel` call therefore survives every test; recorded rather than
/// papered over.
#[tokio::test]
async fn a_view_writes_no_status_row_on_either_server() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewactive").await;

    let before_go = common::status_row(&f.go_user.id).await;
    let before_rust = common::status_row(&f.rust_user.id).await;

    let body = serde_json::json!({ "channel_id": f.channel_id });
    post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    let after_go = common::status_row(&f.go_user.id).await;
    let after_rust = common::status_row(&f.rust_user.id).await;

    cleanup(&http, &f).await;

    assert_eq!(after_go, before_go, "Go wrote a Status row for a view");
    assert_eq!(after_rust, before_rust, "we wrote one");
}

/// **The gate and the body check are in opposite orders on the two routes.** `view` refuses the
/// caller before it looks at the body; `mark_read` parses the body first. Same request, two
/// answers.
#[tokio::test]
async fn the_two_routes_disagree_about_which_refusal_comes_first() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team_id, "vieworder").await;
    let victim = create_plain_user(&http, &admin, &team_id, "vieworderv").await;

    let ((go_view, go_view_body), (rs_view, rs_view_body)) =
        post_both_raw(&http, &plain.token, &view_path(&victim.id), b"not json").await;
    let ((go_mark, go_mark_body), (rs_mark, rs_mark_body)) = post_both_raw(
        &http,
        &plain.token,
        &mark_read_path(&victim.id),
        b"not json",
    )
    .await;

    delete_plain_user(&http, &admin, &plain.id).await;
    delete_plain_user(&http, &admin, &victim.id).await;

    assert_eq!(
        go_view,
        403,
        "view gates first: {}",
        String::from_utf8_lossy(&go_view_body)
    );
    assert_eq!(rs_view, go_view);
    assert_error_bodies_match_except_known_gaps(&go_view_body, &rs_view_body, "view");

    assert_eq!(
        go_mark,
        400,
        "mark_read parses first: {}",
        String::from_utf8_lossy(&go_mark_body)
    );
    assert_eq!(rs_mark, go_mark);
    let go = assert_error_bodies_match_except_known_gaps(&go_mark_body, &rs_mark_body, "mark_read");
    assert_eq!(go["id"], "api.payload.parse.error");
}

/// **`prev_channel_id` is marked read too**, and it is the field a port forgets: every client
/// sends both on a channel switch, and dropping the second one leaves the channel the user just
/// left showing unread for ever.
///
/// Two unread channels, one request. Both must be in `last_viewed_at_times` and both must come
/// back read.
#[tokio::test]
async fn the_previous_channel_is_marked_read_as_well() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let f = fixture(&http, "viewprev").await;

    // A second channel, unread for both users, so the request carries two ids.
    let previous = create_channel(&http, &f.admin, &f.team_id, "viewprev2").await;
    add_user_to_channel(&http, &f.admin, &previous, &f.go_user.id).await;
    add_user_to_channel(&http, &f.admin, &previous, &f.rust_user.id).await;
    post_message(&http, &f.admin, &previous, "mmrs previous channel", None).await;

    let body = serde_json::json!({
        "channel_id": f.channel_id,
        "prev_channel_id": previous,
    });
    let (go_status, go_body) = post_one(&http, GO, &f.go_user.token, &view_path("me"), &body).await;
    let (rs_status, rs_body) =
        post_one(&http, RUST, &f.rust_user.token, &view_path("me"), &body).await;

    let unread_of = async |token: &str, channel: &str| -> i64 {
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
    let go_prev = unread_of(&f.go_user.token, &previous).await;
    let rust_prev = unread_of(&f.rust_user.token, &previous).await;

    cleanup(&http, &f).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "both ids, sorted, in one map"
    );
    let answered: serde_json::Value =
        serde_json::from_slice(&go_body).expect("a ChannelViewResponse");
    assert!(
        answered["last_viewed_at_times"][&previous].is_i64(),
        "the previous channel is in the answer: {answered}"
    );
    assert_eq!(go_prev, 0, "Go marked the previous channel read");
    assert_eq!(rust_prev, go_prev, "and so did we");
}
