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
///
/// `edit_at` is among them, and it is the only one that is *not* obviously per-request: it is
/// stamped by `UpdatePost` and `update_at` is stamped a moment later by the store, so Go — which
/// runs fewer queries in between — usually reports the two as equal while we report a few
/// milliseconds apart. That is latency, not a wire divergence, so the relationship is asserted
/// separately (`0 < edit_at <= update_at`) rather than the value.
fn normalise_post(post: &serde_json::Value) -> serde_json::Value {
    let mut out = post.clone();
    let object = out.as_object_mut().expect("a post object");
    for key in [
        "id",
        "channel_id",
        "create_at",
        "update_at",
        "edit_at",
        "message",
    ] {
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

/// `PUT /posts/{post_id}` against one server, with the body as given.
async fn put_post(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    let path = format!("/api/v4/posts/{post_id}");
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.text().await.expect("a body"),
        served_by_rust,
    )
}

/// `PUT /posts/{post_id}/patch` against one server.
async fn patch_post_request(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    let path = format!("/api/v4/posts/{post_id}/patch");
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.text().await.expect("a body"),
        served_by_rust,
    )
}

#[tokio::test]
async fn editing_a_post_matches_go_body_for_body() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = common::create_channel_typed(&http, &token, &team, "editg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "editr", "O").await;
    let go_post = post_message(&http, &token, &go_channel, "mmrs before", None).await;
    let rust_post = post_message(&http, &token, &rust_channel, "mmrs before", None).await;

    // A `#hashtag` in the new message is what exercises `ParseHashtags` end to end: the column is
    // derived from the message on every edit, and `hashtags` is on the wire.
    let edited = "mmrs after #edited-tag and ##double";
    let (go_status, go_raw, _) = put_post(
        &http,
        GO,
        &token,
        &go_post,
        &serde_json::json!({"id": go_post, "message": edited}),
    )
    .await;
    let (rust_status, rust_raw, served_by_rust) = put_post(
        &http,
        RUST,
        &token,
        &rust_post,
        &serde_json::json!({"id": rust_post, "message": edited}),
    )
    .await;
    assert!(served_by_rust, "we forwarded the edit: {rust_raw}");

    assert_eq!(go_status, 200, "Go edits: {go_raw}");
    assert_eq!(rust_status, go_status, "the edit status differs");
    // `EncodeJSON` goes through `json.Encoder`, so the body **does** carry a trailing newline —
    // the opposite of the pin route's `ReturnStatusOK`.
    assert!(
        go_raw.ends_with('\n'),
        "Go's edit body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(
        rust_raw.ends_with('\n'),
        go_raw.ends_with('\n'),
        "the edit body's framing differs"
    );

    let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("a post");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("a post");
    assert_eq!(
        normalise_post(&go_body),
        normalise_post(&rust_body),
        "the edited post differs:\n go: {go_body}\nrust: {rust_body}"
    );

    // `ParseHashtags`: the pound run collapses and both tags land, in message order.
    assert_eq!(
        rust_body["hashtags"], "#edited-tag #double",
        "the hashtag column is derived from the new message: {rust_body}"
    );
    for (base, body) in [(GO, &go_body), (RUST, &rust_body)] {
        let edit_at = body["edit_at"].as_i64().unwrap_or(0);
        let update_at = body["update_at"].as_i64().unwrap_or(0);
        assert!(edit_at > 0, "{base}: a message change sets edit_at: {body}");
        assert!(
            edit_at <= update_at,
            "{base}: EditAt is stamped by UpdatePost and UpdateAt by the store after it: {body}"
        );
    }
    assert_eq!(
        rust_body["message"], edited,
        "and the response carries the new message"
    );

    // Read back through the writing server: the answer and the row agree.
    let stored = post_through(&http, RUST, &token, &rust_post).await;
    assert_eq!(stored["message"], edited);
    assert_eq!(stored["hashtags"], "#edited-tag #double");
    assert_eq!(stored["edit_at"], rust_body["edit_at"]);

    // The history row holds the *old* message.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, entries) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(status, 200, "{base}: the edit wrote a history row");
        assert_eq!(entries.len(), 1, "{base}: one row: {entries:?}");
        assert_eq!(
            entries[0]["message"], "mmrs before",
            "{base}: the history row is the old version"
        );
    }

    // **An update can pin.** `updatePost` assigns `IsPinned` straight from the body, so a `PUT`
    // carrying `is_pinned` does what the pin route does.
    let (status, raw, _) = put_post(
        &http,
        RUST,
        &token,
        &rust_post,
        &serde_json::json!({"id": rust_post, "message": edited, "is_pinned": true}),
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
    assert_eq!(body["is_pinned"], true, "a PUT can pin: {body}");

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn patching_a_post_matches_go_and_an_empty_patch_still_writes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = common::create_channel_typed(&http, &token, &team, "patchg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "patchr", "O").await;
    let go_post = post_message(&http, &token, &go_channel, "mmrs patch before", None).await;
    let rust_post = post_message(&http, &token, &rust_channel, "mmrs patch before", None).await;

    let patch = serde_json::json!({"message": "mmrs patch after #patched", "is_pinned": true});
    let (go_status, go_raw, _) = patch_post_request(&http, GO, &token, &go_post, &patch).await;
    let (rust_status, rust_raw, served_by_rust) =
        patch_post_request(&http, RUST, &token, &rust_post, &patch).await;
    assert!(served_by_rust, "we forwarded the patch: {rust_raw}");

    assert_eq!(go_status, 200, "Go patches: {go_raw}");
    assert_eq!(rust_status, go_status, "the patch status differs");
    assert_eq!(rust_raw.ends_with('\n'), go_raw.ends_with('\n'));

    let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("a post");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("a post");
    assert_eq!(
        normalise_post(&go_body),
        normalise_post(&rust_body),
        "the patched post differs:\n go: {go_body}\nrust: {rust_body}"
    );
    assert_eq!(rust_body["is_pinned"], true, "the patch pinned it");
    assert_eq!(rust_body["hashtags"], "#patched");

    // **An empty patch is a 200 that still writes.** `postPatchChecks` skips the age limit for it,
    // and `UpdatePost` runs anyway — so `UpdateAt` moves and a history row appears.
    let before = post_through(&http, RUST, &token, &rust_post).await;
    let (status, raw, _) =
        patch_post_request(&http, RUST, &token, &rust_post, &serde_json::json!({})).await;
    assert_eq!(status, 200, "an empty patch is accepted: {raw}");
    let (go_status, go_raw, _) =
        patch_post_request(&http, GO, &token, &go_post, &serde_json::json!({})).await;
    assert_eq!(go_status, status, "and Go agrees: {go_raw}");

    let after = post_through(&http, RUST, &token, &rust_post).await;
    assert!(
        after["update_at"].as_i64() > before["update_at"].as_i64(),
        "an empty patch still moves update_at"
    );
    assert_eq!(
        after["message"], before["message"],
        "and changes nothing a reader can see"
    );
    assert_eq!(
        after["edit_at"], before["edit_at"],
        "including edit_at, since the message did not change"
    );
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (_, entries) = edit_history_through(&http, base, &token, post).await;
        assert_eq!(
            entries.len(),
            2,
            "{base}: the empty patch added a second history row: {entries:?}"
        );
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn the_edit_routes_refuse_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "editerr", "P").await;
    let post = post_message(&http, &token, &channel, "mmrs edit errors", None).await;

    // The body's `id` must match the path's, and the check is **after** `SanitizeInput` and before
    // any lookup — so it fires even for a post that does not exist.
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            &post,
            &serde_json::json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "message": "x"}),
        )
        .await;
        assert_eq!(status, 400, "{base}: a mismatched id is a 400: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // A body that is not a post at all.
    for base in [GO, RUST] {
        let path = format!("/api/v4/posts/{post}");
        let response = http
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status().as_u16(),
            400,
            "{base}: an undecodable body is a 400"
        );
        let body: serde_json::Value = response.json().await.expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // A post that does not exist is a **403** naming `edit_post`, not a 404 — `GetSinglePost`'s
    // error is thrown away. Same for the patch route.
    let missing = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            missing,
            &serde_json::json!({"id": missing, "message": "x"}),
        )
        .await;
        assert_eq!(status, 403, "{base}: an unknown post is a 403: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.permissions.app_error");

        let (status, raw, _) = patch_post_request(
            &http,
            base,
            &token,
            missing,
            &serde_json::json!({"message": "x"}),
        )
        .await;
        assert_eq!(status, 403, "{base}: and so is patching one: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    // A short id is `RequirePostId`'s 400 on both routes.
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            "short",
            &serde_json::json!({"id": "short", "message": "x"}),
        )
        .await;
        assert_eq!(status, 400, "{base}: a short id is a 400: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }

    // A channel member who is not the author needs `edit_others_posts`, which a plain user does
    // not have. **Private** channel, because a public one grants a team member read access anyway.
    let plain = common::create_plain_user(&http, &token, &team, "edit").await;
    common::add_user_to_channel(&http, &token, &channel, &plain.id).await;
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &plain.token,
            &post,
            &serde_json::json!({"id": post, "message": "mmrs hijacked"}),
        )
        .await;
        assert_eq!(
            status, 403,
            "{base}: a member may not edit someone else's post: {raw}"
        );
        let (status, raw, _) = patch_post_request(
            &http,
            base,
            &plain.token,
            &post,
            &serde_json::json!({"message": "mmrs hijacked"}),
        )
        .await;
        assert_eq!(status, 403, "{base}: nor patch it: {raw}");
    }
    assert_eq!(
        post_through(&http, RUST, &token, &post).await["message"],
        "mmrs edit errors",
        "the refused edits must not have landed"
    );

    common::delete_plain_user(&http, &token, &plain.id).await;
    common::delete_channel(&http, &token, &channel).await;
}

/// An edit whose message carries a `~channel` mention is **forwarded**: `FillInPostProps` resolves
/// the mention into a prop through channel and team lookups this port does not do.
///
/// The point of the test is that the forward is *invisible* to a client — same status, same body —
/// and that it happens for the mention rather than for the route.
#[tokio::test]
async fn an_edit_that_mentions_a_channel_is_forwarded_to_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "editmention", "O").await;
    let post = post_message(&http, &token, &channel, "mmrs mention before", None).await;

    let (status, raw, served_by_rust) = put_post(
        &http,
        RUST,
        &token,
        &post,
        &serde_json::json!({"id": post, "message": "see ~town-square for details"}),
    )
    .await;
    assert_eq!(status, 200, "the forwarded edit still succeeds: {raw}");
    assert!(
        !served_by_rust,
        "a ~channel mention must be forwarded, not answered here: {raw}"
    );
    let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
    assert_eq!(body["message"], "see ~town-square for details");

    // And a message with no mention is answered here, so the forward is about the mention.
    let (status, raw, served_by_rust) = put_post(
        &http,
        RUST,
        &token,
        &post,
        &serde_json::json!({"id": post, "message": "no mention at all"}),
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    assert!(served_by_rust, "this one is ours: {raw}");

    common::delete_channel(&http, &token, &channel).await;
}

/// The three things `UpdatePost` does to a post's props, in one exchange against both servers:
/// replace them wholesale, re-apply the integration identity markers from the old post, and delete
/// `mm_blocks_actions` when the post carries no interactive content to justify it.
#[tokio::test]
async fn an_edit_replaces_props_but_keeps_the_identity_markers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = common::create_channel_typed(&http, &token, &team, "propsg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "propsr", "O").await;
    let go_post = post_message(&http, &token, &go_channel, "mmrs props", None).await;
    let rust_post = post_message(&http, &token, &rust_channel, "mmrs props", None).await;

    // `from_webhook` is one of the five identity markers `PreserveIdentityPropsFrom` re-applies.
    // Hardened mode is off by default, so a plain client really can set it.
    let first = serde_json::json!({
        "from_webhook": "true",
        "mmrs_scratch": "kept for now",
        "mm_blocks_actions": {"act": {"type": "external", "url": "https://example.com"}},
    });
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            post,
            &serde_json::json!({"id": post, "message": "mmrs props", "props": first}),
        )
        .await;
        assert_eq!(status, 200, "{base}: props accepted: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
        assert_eq!(
            body["props"]["from_webhook"], "true",
            "{base}: the prop we set is there: {body}"
        );
        assert_eq!(body["props"]["mmrs_scratch"], "kept for now");
        // **`mm_blocks_actions` is pruned to the actions the content references, and there are
        // none** — so `RefreshInteractiveActionsOnPost` deletes the whole registry.
        assert!(
            body["props"].get("mm_blocks_actions").is_none(),
            "{base}: an unreferenced action registry is deleted: {body}"
        );
    }

    // Now replace the props with an empty map. `SetProps` is wholesale, so `mmrs_scratch` goes —
    // and then `PreserveIdentityPropsFrom` puts `from_webhook` back, because an edit must not be
    // able to strip an integration's identity.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            post,
            &serde_json::json!({"id": post, "message": "mmrs props", "props": {}}),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
        assert!(
            body["props"].get("mmrs_scratch").is_none(),
            "{base}: props are replaced wholesale, not merged: {body}"
        );
        assert_eq!(
            body["props"]["from_webhook"], "true",
            "{base}: the identity marker is re-applied from the old post: {body}"
        );
    }

    // And `null` props mean "leave them alone", which is not the same as `{}`. A scratch prop
    // first, so the assertion can distinguish "kept" from "re-applied by the identity pass" —
    // `from_webhook` alone would survive either way.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            post,
            &serde_json::json!({
                "id": post, "message": "mmrs props", "props": {"mmrs_keep": "yes"},
            }),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");

        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            post,
            &serde_json::json!({"id": post, "message": "mmrs props untouched"}),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
        assert_eq!(
            body["props"]["mmrs_keep"], "yes",
            "{base}: an omitted props map keeps the original's: {body}"
        );
        assert_eq!(
            body["props"]["from_webhook"], "true",
            "{base}: and the identity marker is still there: {body}"
        );
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// `rejectOversizedMessage` measures **runes** against `Store.Post().GetMaxPostSize()`, which is
/// `max(character_maximum_length/4, 16383)` — a deployment artifact, not a constant. The error
/// carries both numbers, so this pins the limit as well as the refusal.
#[tokio::test]
async fn an_oversized_message_is_refused_with_the_same_limit() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "oversize", "O").await;
    let post = post_message(&http, &token, &channel, "mmrs oversize", None).await;

    // One rune over 16383. Multi-byte on purpose: the cap is runes, so a port measuring bytes
    // would refuse this at a quarter of the length.
    let long = "é".repeat(16_384);
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            &post,
            &serde_json::json!({"id": post, "message": long}),
        )
        .await;
        assert_eq!(status, 400, "{base}: an oversized message is a 400: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "model.post.is_valid.message_length.app_error");
    }

    // **The handler's check and `Post::is_valid`'s produce the same id, so the only thing that can
    // tell them apart is the *order*.** `rejectOversizedMessage` runs before the ownership check,
    // so a non-author sending an oversized edit gets the length 400 and not the `edit_others_posts`
    // 403 — which is how a mutation removing the handler's check becomes visible at all. Measured;
    // without this assertion the mutation survives, because the store's own `IsValid` answers with
    // the same `id`, status and params a moment later.
    let plain = common::create_plain_user(&http, &token, &team, "oversz").await;
    common::add_user_to_channel(&http, &token, &channel, &plain.id).await;
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &plain.token,
            &post,
            &serde_json::json!({"id": post, "message": long}),
        )
        .await;
        assert_eq!(
            status, 400,
            "{base}: the length check comes before the ownership check: {raw}"
        );
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "model.post.is_valid.message_length.app_error",
            "{base}: a non-author gets the length error, not the 403: {body}"
        );
    }
    common::delete_plain_user(&http, &token, &plain.id).await;

    // And one rune under the cap is accepted, which is what makes the number above the limit and
    // not merely "big".
    let just_under = "é".repeat(16_383);
    let (status, raw, served_by_rust) = put_post(
        &http,
        RUST,
        &token,
        &post,
        &serde_json::json!({"id": post, "message": just_under}),
    )
    .await;
    assert_eq!(
        status,
        200,
        "16383 runes is accepted: {}",
        &raw[..80.min(raw.len())]
    );
    assert!(served_by_rust, "and it is ours");

    common::delete_channel(&http, &token, &channel).await;
}

/// `DELETE /posts/{post_id}` against one server, with an optional `?permanent=` value.
async fn delete_post_request(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
    permanent: Option<&str>,
) -> (u16, String, bool) {
    let path = match permanent {
        Some(value) => format!("/api/v4/posts/{post_id}?permanent={value}"),
        None => format!("/api/v4/posts/{post_id}"),
    };
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.text().await.expect("a body"),
        served_by_rust,
    )
}

/// A post read back with `?include_deleted=true`, which only a `manage_system` caller may do.
/// Returns `(status, body)` so a caller can assert the post is gone as well as what it looks like.
async fn deleted_post_through(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, serde_json::Value) {
    let response = http
        .get(format!(
            "{base}/api/v4/posts/{post_id}?include_deleted=true"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the post route answers");
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn deleting_a_root_post_matches_go_and_takes_its_replies_with_it() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = common::create_channel_typed(&http, &token, &team, "delg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "delr", "O").await;

    let mut roots = Vec::new();
    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        let root = post_message(&http, &token, channel, "mmrs delete root", None).await;
        let reply = post_message(&http, &token, channel, "mmrs delete reply", Some(&root)).await;
        roots.push((base, root, reply));
    }

    let (_, go_root, go_reply) = roots[0].clone();
    let (_, rust_root, rust_reply) = roots[1].clone();

    let (go_status, go_raw, _) = delete_post_request(&http, GO, &token, &go_root, None).await;
    let (rust_status, rust_raw, served_by_rust) =
        delete_post_request(&http, RUST, &token, &rust_root, None).await;
    assert!(served_by_rust, "we forwarded the delete: {rust_raw}");

    assert_eq!(go_status, 200, "Go deletes: {go_raw}");
    assert_eq!(rust_status, go_status, "the delete status differs");
    // `ReturnStatusOK` again — no trailing newline.
    assert_eq!(go_raw, r#"{"status":"OK"}"#);
    assert_eq!(rust_raw, go_raw, "the delete body differs");

    // A soft delete: the row is still there, and only a `manage_system` caller can see it.
    for (base, root) in [(GO, &go_root), (RUST, &rust_root)] {
        let (status, _) = deleted_post_through(&http, base, &token, root).await;
        assert_eq!(status, 200, "{base}: the row survives the delete");

        let plain = http
            .get(format!("{base}/api/v4/posts/{root}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("a response");
        assert_eq!(
            plain.status().as_u16(),
            404,
            "{base}: and is invisible without include_deleted"
        );
    }

    let (_, go_deleted) = deleted_post_through(&http, GO, &token, &go_root).await;
    let (_, rust_deleted) = deleted_post_through(&http, RUST, &token, &rust_root).await;
    // `delete_at` is a per-request timestamp like the ones `normalise_post` already masks; its
    // *relationship* to `update_at` is asserted below instead of its value.
    let mut go_normalised = normalise_post(&go_deleted);
    let mut rust_normalised = normalise_post(&rust_deleted);
    for normalised in [&mut go_normalised, &mut rust_normalised] {
        normalised["delete_at"] = serde_json::json!("<delete_at>");
    }
    assert_eq!(
        go_normalised, rust_normalised,
        "the deleted post differs:\n go: {go_deleted}\nrust: {rust_deleted}"
    );

    for (base, deleted) in [(GO, &go_deleted), (RUST, &rust_deleted)] {
        assert_ne!(
            deleted["delete_at"], 0,
            "{base}: DeleteAt is stamped: {deleted}"
        );
        assert_eq!(
            deleted["delete_at"], deleted["update_at"],
            "{base}: one timestamp is written to both columns: {deleted}"
        );
        // `jsonb_set(props, '{deleteBy}', '"<id>"')` — the deleter's id, in the post's own props.
        assert_eq!(
            deleted["props"]["deleteBy"], me,
            "{base}: props.deleteBy names the deleter: {deleted}"
        );
    }

    // **The reply went with it.** One statement, `WHERE Id = $4 OR RootId = $4`.
    for (base, reply) in [(GO, &go_reply), (RUST, &rust_reply)] {
        let (status, deleted) = deleted_post_through(&http, base, &token, reply).await;
        assert_eq!(status, 200, "{base}: the reply's row survives too");
        assert_ne!(
            deleted["delete_at"], 0,
            "{base}: deleting a root deletes its replies: {deleted}"
        );
        assert_eq!(
            deleted["props"]["deleteBy"], me,
            "{base}: and stamps deleteBy on them: {deleted}"
        );
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// Deleting a **reply** is forwarded: `App.DeletePost` runs `RemoveNotifications` for it, which is
/// the mention engine. A `?permanent=true` delete is forwarded for its own reason — the hard-delete
/// cascade — and both forwards are invisible to the client.
#[tokio::test]
async fn deleting_a_reply_and_a_permanent_delete_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "delfwd", "O").await;
    let root = post_message(&http, &token, &channel, "mmrs fwd root", None).await;
    let reply = post_message(&http, &token, &channel, "mmrs fwd reply", Some(&root)).await;

    let (status, raw, served_by_rust) =
        delete_post_request(&http, RUST, &token, &reply, None).await;
    assert_eq!(status, 200, "the forwarded reply delete succeeds: {raw}");
    assert!(
        !served_by_rust,
        "deleting a reply must be forwarded, not answered here: {raw}"
    );

    // `?permanent=yes` is **not** a true value — `strconv.ParseBool`'s error is discarded — so it
    // takes the ordinary path and is ours.
    let second = post_message(&http, &token, &channel, "mmrs fwd root two", None).await;
    let (status, raw, served_by_rust) =
        delete_post_request(&http, RUST, &token, &second, Some("yes")).await;
    assert_eq!(status, 200, "{raw}");
    assert!(
        served_by_rust,
        "an unparseable permanent flag is false and stays here: {raw}"
    );

    // `?permanent=true` is forwarded, and Go's own gate answers it: `EnableAPIPostDeletion` is off
    // by default, which is a **501**.
    let third = post_message(&http, &token, &channel, "mmrs fwd root three", None).await;
    let (rust_status, rust_raw, served_by_rust) =
        delete_post_request(&http, RUST, &token, &third, Some("true")).await;
    assert!(
        !served_by_rust,
        "a permanent delete must be forwarded: {rust_raw}"
    );
    let fourth = post_message(&http, &token, &channel, "mmrs fwd root four", None).await;
    let (go_status, go_raw, _) =
        delete_post_request(&http, GO, &token, &fourth, Some("true")).await;
    assert_eq!(
        rust_status, go_status,
        "the forwarded permanent delete answers what Go answers: {rust_raw} / {go_raw}"
    );

    common::delete_channel(&http, &token, &channel).await;
}

#[tokio::test]
async fn the_delete_route_refuses_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "delerr", "P").await;

    // A short id is `RequirePostId`'s 400.
    for base in [GO, RUST] {
        let (status, raw, _) = delete_post_request(&http, base, &token, "short", None).await;
        assert_eq!(status, 400, "{base}: a short id is a 400: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }

    // **A post that does not exist is a 404 here**, unlike the pin and edit routes, which turn the
    // same lookup failure into a 403. This route lets `GetSinglePost`'s error through.
    for base in [GO, RUST] {
        let (status, raw, _) =
            delete_post_request(&http, base, &token, "aaaaaaaaaaaaaaaaaaaaaaaaaa", None).await;
        assert_eq!(status, 404, "{base}: an unknown post is a 404: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "app.post.get.app_error");
    }

    let plain = common::create_plain_user(&http, &token, &team, "del").await;
    let admins_post = post_message(&http, &token, &channel, "mmrs admin post", None).await;

    // A non-member cannot delete: the permission check is on the channel.
    for base in [GO, RUST] {
        let (status, raw, _) =
            delete_post_request(&http, base, &plain.token, &admins_post, None).await;
        assert_eq!(status, 403, "{base}: a non-member cannot delete: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    // A **member** still cannot delete somebody else's post: that needs `delete_others_posts`.
    common::add_user_to_channel(&http, &token, &channel, &plain.id).await;
    for base in [GO, RUST] {
        let (status, raw, _) =
            delete_post_request(&http, base, &plain.token, &admins_post, None).await;
        assert_eq!(
            status, 403,
            "{base}: a member cannot delete another user's post: {raw}"
        );
    }
    assert_eq!(
        post_through(&http, RUST, &token, &admins_post).await["delete_at"],
        0,
        "the refused deletes must not have landed"
    );

    // But it can delete its own, which is the `delete_post` arm.
    let own = post_message(&http, &plain.token, &channel, "mmrs own post", None).await;
    let (status, raw, served_by_rust) =
        delete_post_request(&http, RUST, &plain.token, &own, None).await;
    assert_eq!(status, 200, "a member may delete its own post: {raw}");
    assert!(served_by_rust, "and it is ours: {raw}");

    common::delete_plain_user(&http, &token, &plain.id).await;
    common::delete_channel(&http, &token, &channel).await;
}

/// The two `post_deleted` events, and the fact that **which one a client gets depends on
/// `manage_system`**. The admin's socket sees the sensitive one carrying `delete_by`; a plain
/// member's sees the sanitized one, which has no `delete_by` at all.
#[tokio::test]
async fn deleting_a_post_publishes_two_events_to_two_audiences() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "delev", "P").await;
    let plain = common::create_plain_user(&http, &token, &team, "delev").await;
    common::add_user_to_channel(&http, &token, &channel, &plain.id).await;
    let post = post_message(&http, &token, &channel, "mmrs delete event", None).await;

    let mut admin_socket = SocketProbe::connect(RUST, &token).await;
    let mut member_socket = SocketProbe::connect(RUST, &plain.token).await;

    let (status, raw, _) = delete_post_request(&http, RUST, &token, &post, None).await;
    assert_eq!(status, 200, "the delete succeeded: {raw}");

    let arrived = admin_socket
        .collect_until(Duration::from_millis(2_000), |frames| {
            frames.iter().any(|f| f["event"] == "post_deleted")
        })
        .await;
    assert!(
        arrived,
        "no post_deleted on the admin socket: {:?}",
        admin_socket.raw
    );
    member_socket.collect_for(Duration::from_millis(600)).await;

    let admin_events = admin_socket.events_named("post_deleted");
    let member_events = member_socket.events_named("post_deleted");
    assert_eq!(
        admin_events.len(),
        1,
        "the admin gets exactly one of the two: {:?}",
        admin_socket.raw
    );
    assert_eq!(
        member_events.len(),
        1,
        "and so does the member: {:?}",
        member_socket.raw
    );

    // The admin's is the **sensitive** one: it carries who deleted the post.
    assert_eq!(
        admin_events[0]["data"]["delete_by"], me,
        "the admin's event names the deleter: {}",
        admin_events[0]
    );
    assert_eq!(
        admin_events[0]["broadcast"]["contains_sensitive_data"], true,
        "and is tagged as sensitive: {}",
        admin_events[0]
    );
    // The member's is the **sanitized** one, with no `delete_by`.
    assert!(
        member_events[0]["data"].get("delete_by").is_none(),
        "the member's event must not name the deleter: {}",
        member_events[0]
    );
    assert_eq!(
        member_events[0]["broadcast"]["contains_sanitized_data"], true,
        "and is tagged as sanitized: {}",
        member_events[0]
    );

    // Both carry the post as it was **before** the delete.
    let carried: serde_json::Value = admin_events[0]["data"]["post"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .expect("the post is a JSON string");
    assert_eq!(carried["id"], post.as_str());
    assert_eq!(
        carried["delete_at"], 0,
        "the payload is the pre-delete post: {carried}"
    );
    assert!(
        carried["props"].get("deleteBy").is_none(),
        "and has no deleteBy prop yet: {carried}"
    );

    common::delete_plain_user(&http, &token, &plain.id).await;
    common::delete_channel(&http, &token, &channel).await;
}

/// The three cascades a client can see: the flagged-post preference, the thread drafts, and the
/// post's own file infos. Go runs all three from goroutines; this port runs them inline, so the
/// assertions read back through the server that made the write and Go's side is retried.
#[tokio::test]
async fn deleting_a_post_clears_its_flag_its_drafts_and_its_files() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "delcasc", "O").await;

    let file = common::upload_file(
        &http,
        &token,
        &channel,
        "mmrs-delete.txt",
        "text/plain",
        b"mmrs parity delete cascade",
    )
    .await;
    let post = common::post_message_with_files(
        &http,
        &token,
        &channel,
        "mmrs cascade",
        std::slice::from_ref(&file),
    )
    .await;

    // A reply with a file of its own. The two are deleted by **different** statements: the root's
    // by `App.DeletePost`'s own `DeleteForPost`, the reply's by `deleteThreadFiles` inside the
    // store transaction, joined through `Posts.RootId`.
    let reply_file = common::upload_file(
        &http,
        &token,
        &channel,
        "mmrs-delete-reply.txt",
        "text/plain",
        b"mmrs parity delete cascade reply",
    )
    .await;
    let reply_response = http
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel, "root_id": post, "message": "mmrs cascade reply",
            "file_ids": [reply_file],
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        reply_response.status().is_success(),
        "posting the reply with a file failed"
    );

    // Flag it, and start a reply draft in its thread.
    let flagged = http
        .put(format!("{RUST}/api/v4/users/me/preferences"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": me, "category": "flagged_post", "name": post, "value": "true",
        }]))
        .send()
        .await
        .expect("the flag saves");
    assert!(flagged.status().is_success(), "flagging the post failed");

    let drafted = http
        .post(format!("{RUST}/api/v4/drafts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel, "root_id": post, "message": "mmrs cascade draft",
        }))
        .send()
        .await
        .expect("the draft saves");
    assert!(drafted.status().is_success(), "drafting a reply failed");

    let (status, raw, served_by_rust) = delete_post_request(&http, RUST, &token, &post, None).await;
    assert_eq!(status, 200, "the delete succeeded: {raw}");
    assert!(served_by_rust, "and it is ours: {raw}");

    // The flag is gone — `DeleteCategoryAndName` is not scoped to a user, so *everyone's* flag on
    // this post goes, and this reads back the only one there was.
    let prefs: serde_json::Value = http
        .get(format!("{RUST}/api/v4/users/me/preferences/flagged_post"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("preferences are readable")
        .json()
        .await
        .expect("a JSON body");
    assert!(
        !prefs
            .as_array()
            .map(|rows| rows.iter().any(|row| row["name"] == post.as_str()))
            .unwrap_or(false),
        "the flag survived the delete: {prefs}"
    );

    // The thread draft is gone — a hard delete keyed on `(ChannelId, RootId)`.
    let drafts: serde_json::Value = http
        .get(format!("{RUST}/api/v4/users/{me}/teams/{team}/drafts"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("drafts are readable")
        .json()
        .await
        .expect("a JSON body");
    assert!(
        !drafts
            .as_array()
            .map(|rows| rows.iter().any(|row| row["root_id"] == post.as_str()))
            .unwrap_or(false),
        "the thread draft survived the delete: {drafts}"
    );

    // Both file infos are soft-deleted, so `GET /files/{id}/info` no longer finds either — and
    // they got there by different statements, so a port can lose one and keep the other.
    for (which, file_id) in [("the post's own", &file), ("the reply's", &reply_file)] {
        let response = http
            .get(format!("{RUST}/api/v4/files/{file_id}/info"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the file info route answers");
        assert_eq!(
            response.status().as_u16(),
            404,
            "{which} file info survived the delete: {}",
            response.text().await.unwrap_or_default()
        );
    }

    common::delete_channel(&http, &token, &channel).await;
}

/// Plant a post carrying `props` directly, returning its id.
///
/// Local to this module rather than in `common`: it exists for **one** branch, and that branch
/// needs a post whose stored props carry `mm_blocks_actions`, which no route can produce — every
/// write path prunes the registry when the post has no interactive content to justify it, and a
/// post that *has* interactive content is forwarded. So the only way to reach
/// `RefreshInteractiveActionsOnPost`'s delete as something distinct from the preservation branch's
/// is to write the row.
async fn plant_post_with_props(
    channel_id: &str,
    user_id: &str,
    props: &serde_json::Value,
) -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()?;
    let id: String = format!("mmrsplantedprops{:010}", std::process::id());
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query("DELETE FROM posts WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .ok()?;
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid, \
         originalid, message, type, props, hashtags, filenames, fileids, hasreactions, editat, \
         ispinned, remoteid) \
         VALUES ($1, $2, $2, 0, $3, $4, '', '', 'mmrs planted props', '', $5::jsonb, '', '[]', \
         '[]', false, 0, false, '')",
    )
    .bind(&id)
    .bind(now)
    .bind(user_id)
    .bind(channel_id)
    .bind(props.to_string())
    .execute(&pool)
    .await
    .expect("the planted post is written");
    Some(id)
}

/// `RefreshInteractiveActionsOnPost` deletes an action registry the post's content no longer
/// references — and that delete is **distinct** from the one the preservation branch does a few
/// lines earlier, which only fires when the old post had no registry at all.
///
/// Reaching the distinction needs a stored post that carries `mm_blocks_actions` without any
/// interactive content, which no route will write. Planted.
#[tokio::test]
async fn an_edit_prunes_an_action_registry_the_content_no_longer_references() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "prune", "O").await;

    let props = serde_json::json!({
        "mm_blocks_actions": {"act": {"type": "external", "url": "https://example.com"}},
        "mmrs_keep": "1",
    });
    let Some(post) = plant_post_with_props(&channel, me, &props).await else {
        return;
    };

    // The edit carries no `mm_blocks_actions`, so the preservation branch puts the old one back —
    // and then the prune removes it, because the post references no action.
    for base in [GO, RUST] {
        let (status, raw, _) = put_post(
            &http,
            base,
            &token,
            &post,
            &serde_json::json!({"id": post, "message": "mmrs planted props", "props": {"mmrs_keep": "2"}}),
        )
        .await;
        assert_eq!(status, 200, "{base}: the planted post edits: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("a post");
        assert_eq!(body["props"]["mmrs_keep"], "2", "{base}: the edit landed");
        assert!(
            body["props"].get("mm_blocks_actions").is_none(),
            "{base}: the unreferenced registry is pruned, not preserved: {body}"
        );

        // Plant it again for the next server, since the edit consumed it.
        if base == GO {
            common::delete_planted_post(&post).await;
            plant_post_with_props(&channel, me, &props).await;
        }
    }

    common::delete_planted_post(&post).await;
    common::delete_channel(&http, &token, &channel).await;
}

/// The `Threads` row is marked when its root is deleted, and the thread leaves the caller's list.
///
/// `deleteThread` is `UPDATE Threads SET ThreadDeleteAt = ? WHERE PostId = ?` — the row is marked
/// rather than removed, so the thread's reply count and participants survive the delete.
///
/// This test exists because a mutation that disabled that statement outright **survived the whole
/// post-write suite**. Nothing about the stamp reaches the delete response, and every other test
/// here asserts on the response or on the posts themselves, so the one write that is invisible
/// from the route that performs it was also the one nothing checked. It is observable one route
/// over: a thread whose root is deleted stops being listed by `getThreadsForUser`.
///
/// Both the write and the read-back go through the **same** server — Go's caches do not see our
/// writes ([D-190]).
#[tokio::test]
async fn deleting_a_root_post_takes_its_thread_out_of_the_list() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;

    /// The caller's thread roots in a team, read through one server.
    async fn thread_roots(
        http: &reqwest::Client,
        base: &str,
        token: &str,
        user_id: &str,
        team_id: &str,
    ) -> Vec<String> {
        let path = format!("/api/v4/users/{user_id}/teams/{team_id}/threads");
        let response = http
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
        assert_eq!(response.status().as_u16(), 200, "{base}{path} answers 200");
        let body: serde_json::Value = response.json().await.expect("a thread list");
        body["threads"]
            .as_array()
            .map(|threads| {
                threads
                    .iter()
                    .filter_map(|thread| thread["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    // One channel per server: the delete is destructive and the two must not race for a root.
    for base in [GO, RUST] {
        let tag = if base == GO { "thrg" } else { "thrr" };
        let channel = common::create_channel_typed(&http, &token, &team, tag, "O").await;

        // Replying is what creates the `Threads` row and makes the caller a participant, which
        // is what puts it in this list at all.
        let root = post_message(&http, &token, &channel, "mmrs thread root", None).await;
        post_message(&http, &token, &channel, "mmrs thread reply", Some(&root)).await;

        let before = thread_roots(&http, base, &token, me, &team).await;
        assert!(
            before.contains(&root),
            "{base}: a followed thread is listed before its root is deleted: {before:?}"
        );

        let (status, raw, _) = delete_post_request(&http, base, &token, &root, None).await;
        assert_eq!(status, 200, "{base}: the root deletes: {raw}");

        let after = thread_roots(&http, base, &token, me, &team).await;
        assert!(
            !after.contains(&root),
            "{base}: a thread whose root is deleted is no longer listed — \
             `Threads.ThreadDeleteAt` was not stamped: {after:?}"
        );

        common::delete_channel(&http, &token, &channel).await;
    }
}
