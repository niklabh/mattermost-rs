//! Cross-server parity for `POST /api/v4/users/{user_id}/typing`.
//!
//! ```sh
//! scripts/parity.sh --test parity typing
//! ```
//!
//! # The evidence is on the socket, not in the body
//!
//! The body is `{"status":"OK"}` whatever was typed, so every semantic test here holds a
//! websocket open **as a channel member who is not the typist** on each server, posts the same
//! request to both, and compares the `typing` frame each server delivers — `data` and
//! `broadcast` as value graphs. The typist's own socket is held too, to assert the opposite:
//! `omit_users` keeps the event off it.
//!
//! Each server's hub only knows its own connections, so a probe on Go sees Go's publish and a
//! probe on us sees ours; the request is sent to the server whose probe is being asserted.
//!
//! # Every test holds `BUSY_STATE` — read guards, and a write guard for the busy test
//!
//! One test marks this server busy, and this is the only `DisableWhenBusy` route this server
//! serves — so every other test in the file would see its 503. The lock is the one
//! `local_mode`'s busy tests take; holding it here is what keeps a 503 from landing in a test
//! that asserts a 400. Measured: four tests failed that way on the first run.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, BUSY_STATE, GO, RUST, SocketProbe, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel_typed, create_plain_user,
    create_team, go_minted_token, logged_in_user_id, post_message, purge_api_fixtures,
    stack_enabled,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    /// A private channel neither plain user is in.
    closed_id: String,
    /// A root post in `channel_id`, for `parent_id`.
    root_id: String,
    /// In the channel; the typist of most tests.
    typist: common::PlainUser,
    /// In the channel; the listener whose socket the events are asserted on.
    listener: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "typ").await;
            let channel_id = create_channel_typed(client, token, &team_id, "typ", "O").await;
            let closed_id = create_channel_typed(client, token, &team_id, "typshut", "P").await;
            let typist = create_plain_user(client, token, &team_id, "typa").await;
            let listener = create_plain_user(client, token, &team_id, "typb").await;
            add_user_to_channel(client, token, &channel_id, &typist.id).await;
            add_user_to_channel(client, token, &channel_id, &listener.id).await;
            let root_id = post_message(client, token, &channel_id, "root", None).await;
            Fixture {
                team_id,
                channel_id,
                closed_id,
                root_id,
                typist,
                listener,
            }
        })
        .await
}

fn typing_path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/typing")
}

async fn post_typing(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .post(format!("{base}{}", typing_path(user_id)))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} typing is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body").to_vec(),
        served_by,
    )
}

fn request(channel_id: &str, parent_id: &str) -> Vec<u8> {
    serde_json::json!({"channel_id": channel_id, "parent_id": parent_id})
        .to_string()
        .into_bytes()
}

/// The `typing` frames on a probe that carry this channel, so a stranger's typing elsewhere in
/// the suite cannot satisfy the wait or the count.
fn typing_in(channel_id: &str) -> impl Fn(&[serde_json::Value]) -> bool + '_ {
    move |frames| {
        frames
            .iter()
            .any(|f| f["event"] == "typing" && f["broadcast"]["channel_id"] == channel_id)
    }
}

fn typing_frames(probe: &SocketProbe, channel_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named("typing")
        .into_iter()
        .filter(|f| f["broadcast"]["channel_id"] == channel_id)
        .collect()
}

/// Type once on each server as `typist`, with `listener` and `typist` each holding a socket on
/// that server; returns `(go_frame, rust_frame)` as seen by the listener, after asserting the
/// bodies, the statuses, and that the typist's own sockets saw nothing.
async fn type_on_both(
    client: &reqwest::Client,
    f: &Fixture,
    typist_token: &str,
    user_id: &str,
    body: &[u8],
) -> (serde_json::Value, serde_json::Value) {
    let mut go_listener = SocketProbe::connect(GO, &f.listener.token).await;
    let mut rs_listener = SocketProbe::connect(RUST, &f.listener.token).await;
    let mut go_typist = SocketProbe::connect(GO, &f.typist.token).await;
    let mut rs_typist = SocketProbe::connect(RUST, &f.typist.token).await;

    let (go_status, go_body, _) = post_typing(client, GO, typist_token, user_id, body).await;
    let (rs_status, rs_body, served) = post_typing(client, RUST, typist_token, user_id, body).await;
    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_body, rs_body, "the success body differs");
    assert_eq!(rs_body, br#"{"status":"OK"}"#, "ReturnStatusOK, no newline");

    assert!(
        go_listener
            .collect_until(Duration::from_millis(2000), typing_in(&f.channel_id))
            .await,
        "Go delivered no typing frame: {:?}",
        go_listener.raw
    );
    assert!(
        rs_listener
            .collect_until(Duration::from_millis(2000), typing_in(&f.channel_id))
            .await,
        "we delivered no typing frame: {:?}",
        rs_listener.raw
    );
    // The opposite assertion needs a window rather than a wait.
    go_typist.collect_for(Duration::from_millis(600)).await;
    rs_typist.collect_for(Duration::from_millis(600)).await;

    let go_frames = typing_frames(&go_listener, &f.channel_id);
    let rs_frames = typing_frames(&rs_listener, &f.channel_id);
    assert_eq!(
        go_frames.len(),
        1,
        "Go published one: {:?}",
        go_listener.raw
    );
    assert_eq!(
        rs_frames.len(),
        1,
        "we published one: {:?}",
        rs_listener.raw
    );
    assert!(
        typing_frames(&go_typist, &f.channel_id).is_empty(),
        "Go sent the typist their own typing: {:?}",
        go_typist.raw
    );
    assert!(
        typing_frames(&rs_typist, &f.channel_id).is_empty(),
        "we sent the typist their own typing: {:?}",
        rs_typist.raw
    );

    (go_frames[0].clone(), rs_frames[0].clone())
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The frame the listener gets: channel-addressed, typist omitted, exactly `parent_id` and
/// `user_id` in the data — identical on both.
#[tokio::test]
async fn typing_publishes_one_channel_frame_to_members_and_none_to_the_typist() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rs) = type_on_both(
        &client,
        f,
        &f.typist.token,
        &f.typist.id,
        &request(&f.channel_id, &f.root_id),
    )
    .await;

    assert_eq!(go["data"], rs["data"], "the typing data differs");
    assert_eq!(go["broadcast"], rs["broadcast"], "the addressing differs");
    assert_eq!(rs["data"]["parent_id"], f.root_id.as_str());
    assert_eq!(rs["data"]["user_id"], f.typist.id.as_str());
    assert_eq!(rs["data"].as_object().map(|o| o.len()), Some(2));
    assert_eq!(rs["broadcast"]["channel_id"], f.channel_id.as_str());
    assert_eq!(rs["broadcast"]["team_id"], "");
    assert_eq!(rs["broadcast"]["user_id"], "");
    assert_eq!(rs["broadcast"]["omit_users"][&f.typist.id], true);
    assert_eq!(
        rs["broadcast"]["omit_users"].as_object().map(|o| o.len()),
        Some(1)
    );
}

/// An empty `parent_id` is carried as `""`, and `me` resolves to the session user.
#[tokio::test]
async fn an_empty_parent_id_and_the_me_alias() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rs) = type_on_both(
        &client,
        f,
        &f.typist.token,
        "me",
        &request(&f.channel_id, ""),
    )
    .await;
    assert_eq!(go["data"], rs["data"]);
    assert_eq!(go["broadcast"], rs["broadcast"]);
    assert_eq!(rs["data"]["parent_id"], "");
    assert_eq!(rs["data"]["user_id"], f.typist.id.as_str());
}

/// Typing *as* someone else takes `manage_system`: the admin may type as the plain user — and
/// the frame then names the plain user and omits them, not the admin — while the plain user
/// typing as the admin is a 403 on both.
#[tokio::test]
async fn typing_as_another_user_needs_manage_system() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = request(&f.channel_id, "");
    let (go_status, go_body, _) =
        post_typing(&client, GO, &f.typist.token, logged_in_user_id(), &body).await;
    let (rs_status, rs_body, served) =
        post_typing(&client, RUST, &f.typist.token, logged_in_user_id(), &body).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    assert_eq!(served.as_deref(), Some("rust"));
    // The permission's name is in Go's `DetailedError`, which is blanked for a non-admin caller
    // before it reaches the wire, so the id and the status are all a client can compare.
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "typing as admin");
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    // The admin typing as the plain user: allowed, and it is the plain user's typing.
    let (go, rs) = type_on_both(&client, f, &token, &f.typist.id, &body).await;
    assert_eq!(go["data"], rs["data"]);
    assert_eq!(go["broadcast"], rs["broadcast"]);
    assert_eq!(rs["data"]["user_id"], f.typist.id.as_str());
    assert_eq!(rs["broadcast"]["omit_users"][&f.typist.id], true);
}

/// `HasPermissionToChannel(…, create_post)`: a channel the typist is not in, an empty channel
/// id and an unknown one are all the same 403 naming `create_post` — not a 400 or a 404.
#[tokio::test]
async fn typing_needs_create_post_in_the_channel() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (label, body) in [
        (
            "a private channel the typist is not in",
            request(&f.closed_id, ""),
        ),
        ("an empty channel id", request("", "")),
        (
            "an unknown channel id",
            request("zzzzzzzzzzzzzzzzzzzzzzzzzz", ""),
        ),
        // `Decode` of `null` into a struct is not an error: the zero request, so the 403.
        ("a null body", b"null".to_vec()),
    ] {
        let (go_status, go_body, _) =
            post_typing(&client, GO, &f.typist.token, &f.typist.id, &body).await;
        let (rs_status, rs_body, served) =
            post_typing(&client, RUST, &f.typist.token, &f.typist.id, &body).await;
        assert_eq!(
            go_status,
            403,
            "{label}: Go {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            403,
            "{label}: us {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(served.as_deref(), Some("rust"), "{label}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        assert_eq!(parsed["id"], "api.context.permissions.app_error", "{label}");
    }
}

/// A body that does not decode is `SetInvalidParamWithErr("typing_request", …)`, and it is
/// answered **before** the permission checks: the plain user typing as the admin with a broken
/// body gets the 400, not the 403.
#[tokio::test]
async fn a_body_that_does_not_decode_is_a_400_before_permissions() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (label, user_id, body) in [
        ("garbage as self", f.typist.id.as_str(), &b"nope"[..]),
        ("empty body as self", f.typist.id.as_str(), &b""[..]),
        ("garbage as the admin", logged_in_user_id(), &b"{"[..]),
        ("an array as self", f.typist.id.as_str(), &b"[]"[..]),
    ] {
        let (go_status, go_body, _) =
            post_typing(&client, GO, &f.typist.token, user_id, body).await;
        let (rs_status, rs_body, served) =
            post_typing(&client, RUST, &f.typist.token, user_id, body).await;
        assert_eq!(
            go_status,
            400,
            "{label}: Go {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            400,
            "{label}: us {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(served.as_deref(), Some("rust"), "{label}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "{label}"
        );
    }

    // Go's decoder reads one value and stops: trailing bytes after the object are not an error.
    let mut body = request(&f.channel_id, "");
    body.extend_from_slice(b" trailing");
    let (go_status, _, _) = post_typing(&client, GO, &f.typist.token, &f.typist.id, &body).await;
    let (rs_status, _, _) = post_typing(&client, RUST, &f.typist.token, &f.typist.id, &body).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
}

/// `RequireUserId`: a malformed id is a 400 naming `user_id` on both; a segment outside the mux
/// class is Go's own 404, which we forward.
#[tokio::test]
async fn an_invalid_user_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let body = request(&f.channel_id, "");

    let (go_status, go_body, _) = post_typing(&client, GO, &f.typist.token, "short", &body).await;
    let (rs_status, rs_body, served) =
        post_typing(&client, RUST, &f.typist.token, "short", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "short id");
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");

    let (go_status, go_body, _) =
        post_typing(&client, GO, &f.typist.token, "not-an-id", &body).await;
    let (rs_status, rs_body, served) =
        post_typing(&client, RUST, &f.typist.token, "not-an-id", &body).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    assert_eq!(
        served.as_deref(),
        Some("go"),
        "outside the mux class: forwarded, so Go's own 404"
    );
    let parsed = assert_forwarded_bodies_match(&go_body, &rs_body, "mux 404");
    assert_eq!(parsed["id"], "api.context.404.app_error");
}

/// A forwarded answer is Go's own body on both sides, translated message included, so the
/// D-092 exemption `assert_error_bodies_match_except_known_gaps` applies does not fit: compare
/// the two as values with only `request_id` removed.
fn assert_forwarded_bodies_match(
    go_body: &[u8],
    rs_body: &[u8],
    context: &str,
) -> serde_json::Value {
    let mut go: serde_json::Value = serde_json::from_slice(go_body).expect("Go's body is JSON");
    let mut rs: serde_json::Value =
        serde_json::from_slice(rs_body).expect("the forwarded body is JSON");
    for body in [&mut go, &mut rs] {
        if let Some(object) = body.as_object_mut() {
            object.remove("request_id");
        }
    }
    assert_eq!(go, rs, "{context}: the forwarded body is not Go's");
    rs
}

/// `APISessionRequiredDisableWhenBusy`: while this server is busy the route is a 503 with
/// `api.context.server_busy.app_error`, answered after authentication and before the body.
///
/// **Measured on this server only.** Marking the *Go* server busy would refuse every
/// `DisableWhenBusy` route — `createPost` among them — for every suite running beside this one,
/// so the Go half of the comparison is the source (web/handlers.go:349, web/context.go:264)
/// rather than a request. The busy flag is per process ([D-320]).
#[tokio::test]
async fn a_busy_server_refuses_typing_with_a_503() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let set = client
        .post(format!("{RUST}/api/v4/server_busy?seconds=30"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        set.status().as_u16(),
        200,
        "{}",
        set.text().await.unwrap_or_default()
    );

    let (status, body, served) = post_typing(
        &client,
        RUST,
        &f.typist.token,
        &f.typist.id,
        &request(&f.channel_id, ""),
    )
    .await;

    // Clear before asserting, so a failure does not leave the server busy for its neighbours.
    let clear = client
        .delete(format!("{RUST}/api/v4/server_busy"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(clear.status().as_u16(), 200);

    assert_eq!(status, 503, "{}", String::from_utf8_lossy(&body));
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(parsed["id"], "api.context.server_busy.app_error");
    assert_eq!(parsed["status_code"], 503);

    // And a bad token on a busy server is still the 401, not the 503: the gate runs after auth.
    let set = client
        .post(format!("{RUST}/api/v4/server_busy?seconds=30"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(set.status().as_u16(), 200);
    let (status, _, _) = post_typing(
        &client,
        RUST,
        "nottoken",
        &f.typist.id,
        &request(&f.channel_id, ""),
    )
    .await;
    let clear = client
        .delete(format!("{RUST}/api/v4/server_busy"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(clear.status().as_u16(), 200);
    assert_eq!(status, 401);

    // Idle again: the same request succeeds.
    let (status, _, _) = post_typing(
        &client,
        RUST,
        &f.typist.token,
        &f.typist.id,
        &request(&f.channel_id, ""),
    )
    .await;
    assert_eq!(status, 200);
    let _ = &f.team_id;
}
