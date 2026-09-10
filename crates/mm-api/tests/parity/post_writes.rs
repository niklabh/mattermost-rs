//! Cross-server parity for the post writes: `POST /posts/{id}/pin` and `POST /posts/{id}/unpin`.
//!
//! **Every assertion about a write reads back through the server that made it** — Go's caches do
//! not see ours and ours do not invalidate Go's ([D-190]). A test that pinned on one server and
//! read on the other would be measuring the cache, not the route.
//!
//! The interesting thing about these two routes is how much a pin *is*: it goes through
//! `PatchPost` → `UpdatePost` → `SqlPostStore.Update`, so it rewrites the row, bumps `UpdateAt`,
//! moves the channel's `LastPostAt` and inserts an edit-history row. Four side effects for a
//! boolean, and the body is `{"status":"OK"}` either way — so the body alone verifies almost
//! nothing and every test here checks state as well.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity post_writes
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, client, go_minted_token,
    post_message, stack_enabled,
};

/// `POST /posts/{post_id}/pin` or `/unpin` against one server.
async fn set_pinned(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
    pinned: bool,
) -> (u16, String) {
    let verb = if pinned { "pin" } else { "unpin" };
    let path = format!("/api/v4/posts/{post_id}/{verb}");
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        // Sent deliberately: `saveIsPinnedPost`'s event has an **empty** `omit_connection_id`, so
        // a client that names its connection is still told about its own pin. A port that threaded
        // this header into the event would silently break the pinning tab.
        .header("Connection-Id", "mmrs-parity-pin-conn")
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST && status != 404 {
        common::assert_served_by_rust(response.headers(), &path);
    }
    (status, response.text().await.expect("a body"))
}

/// One post, read back through `base`.
async fn post_through(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> serde_json::Value {
    let response = http
        .get(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the post is readable");
    assert_eq!(response.status().as_u16(), 200, "{base} serves the post");
    response.json().await.expect("a post")
}

/// `GET /posts/{id}/edit_history` through `base`, as `(status, entries)`. Zero rows is a **404**
/// in Go, not an empty list, so the status is part of the answer.
async fn edit_history_through(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, Vec<serde_json::Value>) {
    let response = http
        .get(format!("{base}/api/v4/posts/{post_id}/edit_history"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the history route answers");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("a JSON body");
    (status, body.as_array().cloned().unwrap_or_default())
}

async fn channel_through(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
) -> serde_json::Value {
    let response = http
        .get(format!("{base}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the channel is readable");
    response.json().await.expect("a channel")
}

/// Replace the per-request and per-fixture values with markers so two servers' posts compare.
fn normalise_post(post: &serde_json::Value) -> serde_json::Value {
    let mut out = post.clone();
    let object = out.as_object_mut().expect("a post object");
    for key in ["id", "channel_id", "create_at", "update_at", "message"] {
        object.insert(key.to_owned(), serde_json::json!(format!("<{key}>")));
    }
    out
}

#[tokio::test]
async fn pinning_a_post_matches_go_and_writes_an_edit_history_row() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    // A channel each, so neither server's write moves the other's `LastPostAt`.
    let go_channel = common::create_channel_typed(&http, &token, &team, "ping", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "pinr", "O").await;
    let go_post = post_message(&http, &token, &go_channel, "mmrs pin go", None).await;
    let rust_post = post_message(&http, &token, &rust_channel, "mmrs pin rust", None).await;

    // A post that has never been edited has **no** history: 404, not `[]`.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, _) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(
            status, 404,
            "{base}: an unedited post has no edit history yet"
        );
    }

    let before_go = post_through(&http, GO, &token, &go_post).await;
    let before_rust = post_through(&http, RUST, &token, &rust_post).await;
    assert_eq!(before_rust["is_pinned"], false);

    let (go_status, go_body) = set_pinned(&http, GO, &token, &go_post, true).await;
    let (rust_status, rust_body) = set_pinned(&http, RUST, &token, &rust_post, true).await;

    assert_eq!(go_status, 200, "Go pins: {go_body}");
    assert_eq!(rust_status, go_status, "the pin status differs");
    // `ReturnStatusOK` uses `w.Write`, so there is no trailing newline here — unlike every route
    // that goes through `json.Encoder`.
    assert_eq!(go_body, r#"{"status":"OK"}"#, "Go's body is unframed");
    assert_eq!(rust_body, go_body, "the pin body differs");

    let after_go = post_through(&http, GO, &token, &go_post).await;
    let after_rust = post_through(&http, RUST, &token, &rust_post).await;

    assert_eq!(after_rust["is_pinned"], true, "our pin did not land");
    assert_eq!(
        normalise_post(&after_go),
        normalise_post(&after_rust),
        "the pinned post differs:\n go: {after_go}\nrust: {after_rust}"
    );

    // `UpdateAt` moves and `EditAt` does **not**: the message did not change, the file ids did
    // not change and the attachments are equal, so neither `EditAt` assignment fires.
    for (base, before, after) in [
        (GO, &before_go, &after_go),
        (RUST, &before_rust, &after_rust),
    ] {
        assert!(
            after["update_at"].as_i64() > before["update_at"].as_i64(),
            "{base}: a pin must move update_at"
        );
        assert_eq!(
            after["edit_at"], before["edit_at"],
            "{base}: a pin must not set edit_at"
        );
        assert_eq!(after["edit_at"], 0, "{base}: and it was zero to begin with");
    }

    // The history row. This is the assertion that catches a port which "helpfully" updated the
    // `IsPinned` column instead of going through `SqlPostStore.Update`.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, entries) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(status, 200, "{base}: pinning wrote an edit-history row");
        assert_eq!(entries.len(), 1, "{base}: exactly one row: {entries:?}");
        assert_eq!(
            entries[0]["original_id"],
            post.as_str(),
            "{base}: the history row points back at the live post"
        );
        assert_eq!(
            entries[0]["is_pinned"], false,
            "{base}: the history row is the *old* version, still unpinned"
        );
        assert_ne!(
            entries[0]["delete_at"], 0,
            "{base}: the history row carries a DeleteAt"
        );
    }

    // And the pinned-posts route sees it, read through the same server.
    let pinned: serde_json::Value = http
        .get(format!("{RUST}/api/v4/channels/{rust_channel}/pinned"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the pinned list is readable")
        .json()
        .await
        .expect("a post list");
    assert_eq!(
        pinned["order"],
        serde_json::json!([rust_post]),
        "the pinned list should hold exactly our post: {pinned}"
    );

    // Unpin, and the second history row appears.
    let (go_status, go_body) = set_pinned(&http, GO, &token, &go_post, false).await;
    let (rust_status, rust_body) = set_pinned(&http, RUST, &token, &rust_post, false).await;
    assert_eq!(go_status, 200, "Go unpins: {go_body}");
    assert_eq!(rust_status, go_status, "the unpin status differs");
    assert_eq!(rust_body, go_body);
    assert_eq!(
        post_through(&http, RUST, &token, &rust_post).await["is_pinned"],
        false,
        "our unpin did not land"
    );
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (_, entries) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(
            entries.len(),
            2,
            "{base}: unpinning adds a second history row: {entries:?}"
        );
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn pinning_an_already_pinned_post_writes_nothing() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = common::create_channel_typed(&http, &token, &team, "noopg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "noopr", "O").await;
    let go_post = post_message(&http, &token, &go_channel, "mmrs noop go", None).await;
    let rust_post = post_message(&http, &token, &rust_channel, "mmrs noop rust", None).await;

    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        assert_eq!(set_pinned(&http, base, &token, post, true).await.0, 200);
    }
    let once_go = post_through(&http, GO, &token, &go_post).await;
    let once_rust = post_through(&http, RUST, &token, &rust_post).await;

    // The short circuit: `post.IsPinned == isPinned` answers OK *before* the time-limit check and
    // before `PatchPost`, so nothing is written at all.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, body) = set_pinned(&http, base, &token, post, true).await;
        assert_eq!(status, 200, "{base}: a repeat pin is still OK: {body}");
        assert_eq!(body, r#"{"status":"OK"}"#);
    }

    for (base, once, post) in [(GO, &once_go, &go_post), (RUST, &once_rust, &rust_post)] {
        let twice = post_through(&http, base, &token, post).await;
        assert_eq!(
            twice["update_at"], once["update_at"],
            "{base}: a repeat pin must not touch the row"
        );
        let (_, entries) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(
            entries.len(),
            1,
            "{base}: and must not add a history row: {entries:?}"
        );
    }

    // The same short circuit on the other side: unpinning a post that was never pinned.
    let fresh = post_message(&http, &token, &rust_channel, "mmrs never pinned", None).await;
    let (status, body) = set_pinned(&http, RUST, &token, &fresh, false).await;
    assert_eq!(status, 200, "unpinning an unpinned post is OK: {body}");
    let (status, _) = edit_history_through(&http, RUST, &token, &fresh).await;
    assert_eq!(status, 404, "and wrote no history row");

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn a_pin_publishes_post_edited_and_moves_the_channel() {
    if !stack_enabled() {
        return;
    }
    // Asserts a *count* of frames on the shared admin's stream — see `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "pinev", "O").await;
    let post = post_message(&http, &token, &channel, "mmrs pin event", None).await;
    let before = channel_through(&http, RUST, &token, &channel).await;

    let mut socket = SocketProbe::connect(RUST, &token).await;

    let (status, body) = set_pinned(&http, RUST, &token, &post, true).await;
    assert_eq!(status, 200, "the pin succeeded: {body}");

    let arrived = socket
        .collect_until(Duration::from_millis(2_000), |frames| {
            frames.iter().any(|frame| frame["event"] == "post_edited")
        })
        .await;
    assert!(arrived, "no post_edited arrived: {:?}", socket.raw);

    let edited = socket.events_named("post_edited");
    assert_eq!(edited.len(), 1, "exactly one post_edited: {:?}", socket.raw);
    let event = &edited[0];
    assert_eq!(
        event["broadcast"]["channel_id"],
        channel.as_str(),
        "the event is channel-scoped: {event}"
    );
    assert_eq!(
        event["broadcast"]["user_id"], "",
        "and names no user: {event}"
    );
    // **The header was sent and the event still arrived.** `NewWebSocketEvent(..., "")` leaves
    // `omit_connection_id` empty, so the pinning client is not skipped.
    assert_eq!(
        event["broadcast"]["omit_connection_id"], "",
        "the pin event omits no connection: {event}"
    );

    // The post rides as a JSON **string** under `post`, as it does in every post event.
    let carried: serde_json::Value = event["data"]["post"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .expect("the post is a JSON string");
    assert_eq!(carried["id"], post.as_str());
    assert_eq!(
        carried["is_pinned"], true,
        "the event carries the new state"
    );
    assert!(
        carried.get("is_following").is_none() || carried["is_following"].is_null(),
        "is_following is nulled before the broadcast: {carried}"
    );

    // `SqlPostStore.Update`'s second statement: the channel's `LastPostAt` moves to now, so an
    // edited old post reorders the sidebar.
    let after = channel_through(&http, RUST, &token, &channel).await;
    assert!(
        after["last_post_at"].as_i64() > before["last_post_at"].as_i64(),
        "the pin should move last_post_at: {} -> {}",
        before["last_post_at"],
        after["last_post_at"]
    );
    assert_eq!(
        after["total_msg_count"], before["total_msg_count"],
        "and must not change the message count"
    );

    common::delete_channel(&http, &token, &channel).await;
}

#[tokio::test]
async fn the_pin_routes_refuse_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;

    // A well-formed id for a post that does not exist is a **403**, not a 404:
    // `GetSinglePost`'s error is thrown away and replaced with a permission error.
    for base in [GO, RUST] {
        let (status, body) =
            set_pinned(&http, base, &token, "aaaaaaaaaaaaaaaaaaaaaaaaaa", true).await;
        assert_eq!(
            status, 403,
            "{base}: an unknown post id is a permission error: {body}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("an AppError");
        assert_eq!(parsed["id"], "api.context.permissions.app_error");
    }

    // A short id is `RequirePostId`'s 400. Both servers, same id.
    for base in [GO, RUST] {
        let (status, body) = set_pinned(&http, base, &token, "tooshort", true).await;
        assert_eq!(status, 400, "{base}: a short post id is a 400: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("an AppError");
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    }

    // A post in a private channel the caller is not in. **Private**, because the public fallback
    // would serve a team member — see `create_channel_typed`.
    let private = common::create_channel_typed(&http, &token, &team, "pinpriv", "P").await;
    let hidden = post_message(&http, &token, &private, "mmrs pin hidden", None).await;
    let plain = common::create_plain_user(&http, &token, &team, "pin").await;

    for base in [GO, RUST] {
        let (status, body) = set_pinned(&http, base, &plain.token, &hidden, true).await;
        assert_eq!(status, 403, "{base}: a non-member cannot pin: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("an AppError");
        assert_eq!(parsed["id"], "api.context.permissions.app_error");
    }
    assert_eq!(
        post_through(&http, RUST, &token, &hidden).await["is_pinned"],
        false,
        "the refused pin must not have landed"
    );

    common::delete_plain_user(&http, &token, &plain.id).await;
    common::delete_channel(&http, &token, &private).await;
}

/// A **member** who is not the author, and holds no `edit_post` on somebody else's post, can still
/// pin it — the route's only gate is `read_channel_content`.
#[tokio::test]
async fn a_plain_member_may_pin_someone_elses_post() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "pinmem", "P").await;
    let post = post_message(&http, &token, &channel, "mmrs pin by member", None).await;
    let plain = common::create_plain_user(&http, &token, &team, "pinm").await;
    common::add_user_to_channel(&http, &token, &channel, &plain.id).await;

    let (status, body) = set_pinned(&http, RUST, &token, &post, true).await;
    assert_eq!(status, 200, "the admin pins first: {body}");
    let (status, body) = set_pinned(&http, RUST, &plain.token, &post, false).await;
    assert_eq!(
        status, 200,
        "a plain member may unpin the admin's post: {body}"
    );
    assert_eq!(
        post_through(&http, RUST, &token, &post).await["is_pinned"],
        false,
        "and it landed"
    );

    common::delete_plain_user(&http, &token, &plain.id).await;
    common::delete_channel(&http, &token, &channel).await;
}
