//! Cross-server parity for `POST /api/v4/reactions` and
//! `DELETE /api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}` — **the first two
//! routes that mutate a row and broadcast**.
//!
//! A write is tested on three surfaces, and a port can be wrong on any one of them while looking
//! right on the others:
//!
//! 1. the **answer** — status and body, with the two timestamps normalised;
//! 2. the **row**, read back through *both* servers afterwards, which is what proves the write
//!    landed rather than merely that the handler said so;
//! 3. the **event** on a websocket, which is the half no HTTP comparison can see.
//!
//! Each server writes to its **own** post. Sending the same reaction to both would make the
//! second call an upsert over the first's row, and an upsert that returns the original
//! `create_at` looks exactly like agreement.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity reaction_writes
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, add_user_to_channel, client,
    create_channel_typed, create_custom_emoji, create_plain_user, create_team, delete_channel,
    delete_custom_emoji, delete_plain_user, go_minted_token, logged_in_user_id, login_plain_user,
    post_message, stack_enabled, unique_emoji_name,
};

/// A system emoji, so no fixture is needed for the common path.
const EMOJI: &str = "smile";

/// One reaction write against one server.
async fn react(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    post_id: &str,
    emoji: &str,
) -> (u16, serde_json::Value, String) {
    let response = http
        .post(format!("{base}/api/v4/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "user_id": user_id,
            "post_id": post_id,
            "emoji_name": emoji,
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), "/api/v4/reactions");
    }
    let raw = response.text().await.expect("a body");
    let parsed = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    (status, parsed, raw)
}

async fn unreact(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    post_id: &str,
    emoji: &str,
) -> (u16, String) {
    unreact_maybe_forwarded(http, base, token, user_id, post_id, emoji, true).await
}

/// [`unreact`] with the served-by assertion made optional.
///
/// A segment outside gorilla's character class is *supposed* to be forwarded — Go answers a 404
/// from its router, which this server reproduces by handing the request over — so those cases
/// must not assert that Rust served them.
async fn unreact_maybe_forwarded(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    post_id: &str,
    emoji: &str,
    expect_rust: bool,
) -> (u16, String) {
    let path = format!("/api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji}");
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST && expect_rust {
        common::assert_served_by_rust(response.headers(), &path);
    }
    (status, response.text().await.expect("a body"))
}

/// The reactions on a post, read through one server.
///
/// # Go's answer here is cached, our writes cannot invalidate it, and **nor can `/caches/invalidate`**
///
/// `LocalCacheReactionStore` (localcachelayer/reaction_layer.go:35) memoises `GetForPost` per
/// post. It is purged only by Go's *own* `Save`/`Delete` and by a cluster message — and
/// `InvalidateAllCachesSkipSend` (platform/cluster_handlers.go:137), which is what
/// `POST /api/v4/caches/invalidate` runs, clears Team, Channel, User, Post, FileInfo and Webhook
/// and **not** the reaction cache.
///
/// So on a single node there is no way to make Go re-read reactions from the database. Measured:
/// a reaction deleted through `:8066` had `DeleteAt` set in the table and was absent from
/// `:8066`'s read, while `:8065` still listed it after three explicit invalidations.
///
/// Every assertion about what a *Rust* write did therefore reads through Rust, which reads the
/// database. See [D-190].
async fn reactions_on(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> Vec<serde_json::Value> {
    let response = http
        .get(format!("{base}/api/v4/posts/{post_id}/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("a readable post");
    let value: serde_json::Value = response.json().await.expect("a JSON body");
    value.as_array().cloned().unwrap_or_default()
}

/// Replace the two timestamps with a marker, keeping the fact that they are non-zero and equal to
/// each other — which is what `PreSave` guarantees on a first save and is itself worth asserting.
fn normalise_timestamps(reaction: &serde_json::Value) -> serde_json::Value {
    let mut out = reaction.clone();
    let object = out.as_object_mut().expect("a reaction object");
    let create_at = object
        .get("create_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let update_at = object
        .get("update_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    object.insert(
        "create_at".to_owned(),
        serde_json::json!(if create_at > 0 { "nonzero" } else { "zero" }),
    );
    object.insert(
        "update_at".to_owned(),
        serde_json::json!(match update_at {
            0 => "zero",
            u if u == create_at => "equal-to-create-at",
            _ => "later",
        }),
    );
    out
}

/// Wait for one `event_type` frame naming `post_id`, then keep collecting briefly so a second
/// would also be seen.
///
/// A fixed window used to stand here. It encoded how fast the Go server was, and that changed:
/// the published image ran under qemu and the pinned build runs native, so `both_servers_broadcast_the_same_reaction_event`
/// started reporting *zero* `reaction_added` frames inside 900ms on a server that was strictly
/// faster. Waiting for the event rather than for the clock is what makes the assertion mean the
/// same thing at either speed; the trailing collect keeps "and not a second one" testable.
async fn wait_for_reaction_event(probe: &mut SocketProbe, event_type: &str, post_id: &str) {
    let event_type = event_type.to_owned();
    let post_id = post_id.to_owned();
    probe
        .collect_until(Duration::from_secs(5), move |frames| {
            frames.iter().any(|frame| {
                frame["event"] == event_type.as_str()
                    && frame["data"]["reaction"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                        .is_some_and(|reaction| reaction["post_id"] == post_id.as_str())
            })
        })
        .await;
    probe.collect_for(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn saving_a_reaction_answers_and_persists_the_same_row_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;

    // One post per server: an upsert over a shared row would return the first server's
    // `create_at` and read as agreement.
    let go_post = post_message(&http, &token, &channel, "mmrs reaction parity go", None).await;
    let rust_post = post_message(&http, &token, &channel, "mmrs reaction parity rust", None).await;

    let (go_status, go_body, go_raw) =
        react(&http, GO, &token, logged_in_user_id(), &go_post, EMOJI).await;
    let (rust_status, rust_body, rust_raw) =
        react(&http, RUST, &token, logged_in_user_id(), &rust_post, EMOJI).await;

    assert_eq!(go_status, 200);
    assert_eq!(rust_status, go_status, "the save status differs");

    // `json.NewEncoder(w).Encode` — the success body carries a trailing newline, where the
    // `getReactions` handler ten lines away in the same Go file does not.
    assert!(
        go_raw.ends_with('\n'),
        "Go's saveReaction body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(
        rust_raw.ends_with('\n'),
        go_raw.ends_with('\n'),
        "the save body's framing differs:\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );

    // Everything but the post id and the timestamps must be identical, including `channel_id`,
    // which the handler overwrites from the post rather than taking from the request.
    let mut go_normalised = normalise_timestamps(&go_body);
    let mut rust_normalised = normalise_timestamps(&rust_body);
    go_normalised["post_id"] = serde_json::json!("<post>");
    rust_normalised["post_id"] = serde_json::json!("<post>");
    assert_eq!(
        go_normalised, rust_normalised,
        "the saved reaction differs:\n go: {go_body}\nrust: {rust_body}"
    );
    assert_eq!(
        rust_body["channel_id"], channel,
        "channel_id comes from the post, not from the request body"
    );
    assert_eq!(
        rust_normalised["update_at"], "equal-to-create-at",
        "PreSave sets both from one GetMillis on a first save"
    );

    // The row, read back. This is the assertion that separates "the handler answered" from "the
    // handler wrote".
    //
    // **Each write is read back through the server that made it**, because Go's reaction cache
    // cannot be made to see our write — see `reactions_on`. Reading Go's own write through Go and
    // ours through Rust still compares two independent code paths against the same database, and
    // it is the strongest claim available while both servers run.
    let go_written = reactions_on(&http, GO, &token, &go_post).await;
    let rust_written = reactions_on(&http, RUST, &token, &rust_post).await;
    assert_eq!(go_written.len(), 1, "Go's post should carry one reaction");
    assert_eq!(rust_written.len(), 1, "our post should carry one reaction");
    assert_eq!(go_written[0]["emoji_name"], EMOJI);
    assert_eq!(rust_written[0]["emoji_name"], EMOJI);
    assert_eq!(
        normalise_timestamps(&go_written[0])["delete_at"],
        normalise_timestamps(&rust_written[0])["delete_at"],
    );

    // The rows Go wrote are readable through us, which is the direction the cache does not block:
    // our reads always go to the database.
    let go_post_through_rust = reactions_on(&http, RUST, &token, &go_post).await;
    assert_eq!(
        go_post_through_rust, go_written,
        "Go's write reads back differently through us"
    );

    // And the delete, each through its own server.
    let (go_status, go_raw) =
        unreact(&http, GO, &token, logged_in_user_id(), &go_post, EMOJI).await;
    let (rust_status, rust_raw) =
        unreact(&http, RUST, &token, logged_in_user_id(), &rust_post, EMOJI).await;
    assert_eq!(go_status, 200);
    assert_eq!(rust_status, go_status, "the delete status differs");
    assert_eq!(rust_raw, go_raw, "the delete body differs");
    assert_eq!(
        go_raw, r#"{"status":"OK"}"#,
        "ReturnStatusOK is written with w.Write and carries no newline"
    );

    // Read the *effect* of both deletes through us, for the reason above. Go's own delete is
    // additionally confirmed through Go, where its cache was invalidated by its own write.
    for (label, post_id) in [("go's post", &go_post), ("rust's post", &rust_post)] {
        assert!(
            reactions_on(&http, RUST, &token, post_id).await.is_empty(),
            "{label} still has a reaction after the delete"
        );
    }
    assert!(
        reactions_on(&http, GO, &token, &go_post).await.is_empty(),
        "Go's own delete is not visible to Go"
    );
}

#[tokio::test]
async fn both_servers_broadcast_the_same_reaction_event() {
    if !stack_enabled() {
        return;
    }
    // Serialised against every other broadcast-counting test: this one asserts a *count* of
    // frames on the shared admin's stream, which is only true while nothing else writes to
    // that user. See `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_post = post_message(&http, &token, &channel, "mmrs reaction event go", None).await;
    let rust_post = post_message(&http, &token, &channel, "mmrs reaction event rust", None).await;

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    // `reaction_added` is presence-scoped in the hub: a connection that has told the server it
    // has *another* channel open does not receive it. Neither probe has sent a `presence` frame,
    // so both are unset and the scope does not apply — which is the state a fresh client is in.
    react(&http, GO, &token, logged_in_user_id(), &go_post, EMOJI).await;
    react(&http, RUST, &token, logged_in_user_id(), &rust_post, EMOJI).await;

    wait_for_reaction_event(&mut go_socket, "reaction_added", &go_post).await;
    wait_for_reaction_event(&mut rust_socket, "reaction_added", &rust_post).await;

    // **Scoped to this test's own post.** `events_named` filters by event type alone, and the
    // suite has other tests reacting in the *same* channel — the fifty-emoji limit fixture alone
    // puts more than fifty `reaction_added` frames on this socket. Counting by name was counting
    // them. Same lesson as `SocketProbe::responses`, one level deeper: a socket sees the whole
    // server, so every assertion on it has to name what it is looking for.
    let go_events = reaction_events(&go_socket, "reaction_added", &go_post);
    let rust_events = reaction_events(&rust_socket, "reaction_added", &rust_post);
    assert_eq!(
        go_events.len(),
        1,
        "Go published one reaction_added: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        1,
        "we published one reaction_added: {:?}",
        rust_socket.raw
    );

    // The broadcast block carries the channel and nothing else — no team, no user. That is what
    // makes the event reach every member of the channel rather than only its author.
    assert_eq!(
        go_events[0]["broadcast"], rust_events[0]["broadcast"],
        "the broadcast addressing differs"
    );
    assert_eq!(rust_events[0]["broadcast"]["channel_id"], channel);
    assert_eq!(rust_events[0]["broadcast"]["user_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["team_id"], "");

    // **`data.reaction` is a JSON string, not an object.** Go marshals the reaction and adds the
    // resulting string, so a client decodes twice.
    let go_reaction = go_events[0]["data"]["reaction"]
        .as_str()
        .expect("Go's reaction is a string");
    let rust_reaction = rust_events[0]["data"]["reaction"]
        .as_str()
        .expect("our reaction must be a string, not an object");
    let go_reaction: serde_json::Value = serde_json::from_str(go_reaction).expect("decodes");
    let rust_reaction: serde_json::Value = serde_json::from_str(rust_reaction).expect("decodes");

    let mut go_normalised = normalise_timestamps(&go_reaction);
    let mut rust_normalised = normalise_timestamps(&rust_reaction);
    go_normalised["post_id"] = serde_json::json!("<post>");
    rust_normalised["post_id"] = serde_json::json!("<post>");
    assert_eq!(
        go_normalised, rust_normalised,
        "the reaction carried by the event differs"
    );

    // The precomputed framing: a broadcast event is the spaced form and carries no newline, where
    // the `hello` on the same socket is compact and does.
    let rust_raw = rust_socket
        .raw
        .iter()
        .find(|raw| raw.contains("reaction_added"))
        .expect("the raw frame");
    assert!(
        rust_raw.starts_with(r#"{"event": "#),
        "a broadcast takes Go's precompute path: {rust_raw:?}"
    );
    assert!(
        !rust_raw.ends_with('\n'),
        "the precompute path writes no trailing newline: {rust_raw:?}"
    );

    unreact(&http, GO, &token, logged_in_user_id(), &go_post, EMOJI).await;
    unreact(&http, RUST, &token, logged_in_user_id(), &rust_post, EMOJI).await;

    wait_for_reaction_event(&mut go_socket, "reaction_removed", &go_post).await;
    wait_for_reaction_event(&mut rust_socket, "reaction_removed", &rust_post).await;
    assert_eq!(
        reaction_events(&go_socket, "reaction_removed", &go_post).len(),
        1,
        "Go published one reaction_removed"
    );
    assert_eq!(
        reaction_events(&rust_socket, "reaction_removed", &rust_post).len(),
        1,
        "we published one reaction_removed: {:?}",
        rust_socket.raw
    );
}

/// The reaction events on `probe` of type `event_type` that concern `post_id`.
///
/// The post id is inside `data.reaction`, which is a JSON **string**, so selecting on it means
/// decoding twice — the same double decode a real client does.
fn reaction_events(probe: &SocketProbe, event_type: &str, post_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named(event_type)
        .into_iter()
        .filter(|frame| {
            frame["data"]["reaction"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|reaction| reaction["post_id"].as_str().map(|id| id == post_id))
                .unwrap_or(false)
        })
        .collect()
}

#[tokio::test]
async fn the_four_body_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction refusals", None).await;
    let me = logged_in_user_id();

    // Each case is a refusal, so nothing is written and both servers can see the same request.
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "a malformed id is the compound invalid error, not an id error",
            serde_json::json!({"user_id": me, "post_id": "short", "emoji_name": EMOJI}),
        ),
        (
            "an empty emoji name",
            serde_json::json!({"user_id": me, "post_id": post, "emoji_name": ""}),
        ),
        (
            "an emoji name over 64 bytes",
            serde_json::json!({"user_id": me, "post_id": post, "emoji_name": "a".repeat(65)}),
        ),
        (
            "reacting as somebody else is a 403 before any permission check",
            serde_json::json!({
                "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "post_id": post,
                "emoji_name": EMOJI,
            }),
        ),
    ];

    for (what, body) in cases {
        let go = http
            .post(format!("{GO}/api/v4/reactions"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("Go answers");
        let go_status = go.status().as_u16();
        let go_body: serde_json::Value = go.json().await.expect("an AppError");

        let rust = http
            .post(format!("{RUST}/api/v4/reactions"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("we answer");
        let rust_status = rust.status().as_u16();
        common::assert_served_by_rust(rust.headers(), "/api/v4/reactions");
        let rust_body: serde_json::Value = rust.json().await.expect("an AppError");

        assert_eq!(rust_status, go_status, "{what}: the status differs");
        assert_eq!(
            rust_body["id"], go_body["id"],
            "{what}: the error id differs"
        );
        assert_eq!(
            rust_body["status_code"], go_body["status_code"],
            "{what}: the body's status_code differs"
        );
    }

    // A body that is not JSON at all.
    for (base, expect_rust) in [(GO, false), (RUST, true)] {
        let response = http
            .post(format!("{base}/api/v4/reactions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status().as_u16(), 400, "{base} on a broken body");
        if expect_rust {
            common::assert_served_by_rust(response.headers(), "/api/v4/reactions");
        }
        let body: serde_json::Value = response.json().await.expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }
}

#[tokio::test]
async fn an_unknown_emoji_answers_the_emoji_routes_own_error() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction unknown emoji", None).await;

    // Not wrapped by the reaction layer: `SaveReactionForPost` returns `GetEmojiByName`'s error
    // unchanged, so a client sees the **store**'s id — `app.emoji.get_by_name.no_result`, not the
    // `api.emoji.…` an api4 route would produce — from a reaction route. Measured against the
    // running server; the first version of this test asserted the api-layer id and Go disagreed.
    let name = unique_emoji_name("noemoji");
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &token, logged_in_user_id(), &post, &name).await;
        assert_eq!(status, 404, "{base} should 404 an unknown emoji");
        assert_eq!(
            body["id"], "app.emoji.get_by_name.no_result",
            "{base} returned the wrong error id"
        );
    }

    // And a *custom* emoji is accepted by the same path, which is what proves the system-emoji
    // short-circuit is not the only branch that can succeed.
    let emoji_id = create_custom_emoji(&http, &token, logged_in_user_id(), &name).await;
    let (status, body, _) = react(&http, RUST, &token, logged_in_user_id(), &post, &name).await;
    assert_eq!(status, 200, "a custom emoji is a valid reaction: {body}");
    assert_eq!(body["emoji_name"], name);

    unreact(&http, RUST, &token, logged_in_user_id(), &post, &name).await;
    delete_custom_emoji(&http, &token, &emoji_id).await;
}

#[tokio::test]
async fn removing_another_users_reaction_needs_the_system_permission() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;

    // A private channel, so membership is what grants access — a public one falls back to the
    // team's `read_public_channel` and a "non-member" is served rather than refused.
    let channel = create_channel_typed(&http, &admin, &team, "reactdel", "P").await;
    let plain = create_plain_user(&http, &admin, &team, "reactdel").await;
    add_user_to_channel(&http, &admin, &channel, &plain.id).await;
    let plain_token = login_plain_user(&http, "reactdel").await;

    let post = post_message(&http, &admin, &channel, "mmrs reaction cross-user", None).await;

    // The admin reacts; the plain user tries to remove it. `remove_others_reactions` is a
    // *system* permission, held by system_admin and not by system_user.
    react(&http, GO, &admin, logged_in_user_id(), &post, EMOJI).await;

    let (go_status, go_body) =
        unreact(&http, GO, &plain_token, logged_in_user_id(), &post, EMOJI).await;
    let (rust_status, rust_body) =
        unreact(&http, RUST, &plain_token, logged_in_user_id(), &post, EMOJI).await;
    assert_eq!(go_status, 403, "Go refuses the cross-user delete");
    assert_eq!(
        rust_status, go_status,
        "the cross-user refusal status differs"
    );

    let go_body: serde_json::Value = serde_json::from_str(&go_body).expect("an AppError");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_body).expect("an AppError");
    assert_eq!(
        rust_body["id"], go_body["id"],
        "the cross-user refusal id differs"
    );

    // The admin can, and the reaction really goes.
    let (status, _) = unreact(&http, RUST, &admin, logged_in_user_id(), &post, EMOJI).await;
    assert_eq!(status, 200);
    assert!(reactions_on(&http, RUST, &admin, &post).await.is_empty());

    delete_channel(&http, &admin, &channel).await;
    delete_plain_user(&http, &admin, &plain.id).await;
}

#[tokio::test]
async fn re_reacting_revives_the_row_and_the_answer_disagrees_with_it() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_post = post_message(&http, &token, &channel, "mmrs reaction rereact go", None).await;
    let rust_post = post_message(&http, &token, &channel, "mmrs reaction rereact rust", None).await;
    let me = logged_in_user_id();

    // React, withdraw, react again.
    //
    // **The answer and the row disagree, and that is Go's behaviour.** `PreSave` mints a fresh
    // `CreateAt` because the incoming reaction has none, and the response is marshalled from
    // *that* struct — but the insert is `ON CONFLICT DO UPDATE` over four columns and `CreateAt`
    // is not among them, so the stored row keeps its original. A port that "fixed" either side
    // would diverge from Go on the other. Measured: the first version of this test asserted the
    // answer kept the old value and Go disagreed.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, first, _) = react(&http, base, &token, me, post, EMOJI).await;
        assert_eq!(status, 200, "{base} first react");
        unreact(&http, base, &token, me, post, EMOJI).await;
        let (status, second, _) = react(&http, base, &token, me, post, EMOJI).await;
        assert_eq!(status, 200, "{base} second react is not an error");
        assert!(
            second["create_at"].as_i64() > first["create_at"].as_i64(),
            "{base}: the *answer* carries the fresh PreSave timestamp"
        );
        assert!(
            second["update_at"].as_i64() >= first["update_at"].as_i64(),
            "{base} did not move update_at forward"
        );
        assert_eq!(second["delete_at"], 0, "{base} left the row deleted");

        // The row, which is the half the answer does not describe. Read through the server that
        // wrote it — see `reactions_on` for why Go cannot be asked about our write.
        let stored = reactions_on(&http, base, &token, post).await;
        assert_eq!(stored.len(), 1, "{base}: one revived row, not two");
        assert_eq!(
            stored[0]["create_at"], first["create_at"],
            "{base}: the stored row keeps its original create_at through the revival"
        );
        assert_eq!(stored[0]["delete_at"], 0, "{base}: the row is undeleted");
    }

    unreact(&http, GO, &token, me, &go_post, EMOJI).await;
    unreact(&http, RUST, &token, me, &rust_post, EMOJI).await;
}

#[tokio::test]
async fn the_delete_path_validates_its_three_segments() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction segments", None).await;
    let me = logged_in_user_id();

    // Unlike `saveReaction`, this handler *checks* the result of its three Require calls, so a
    // malformed segment really is a 400 — and each names its own parameter.
    // Two different refusals, and which one you get depends on gorilla, not on the handler.
    //
    // `short` is inside `[A-Za-z0-9]+`, so the route matches and `RequireUserId` answers **400**.
    // `not an emoji` is outside `[A-Za-z0-9\_\-\+]+`, so **no route matches at all** and Go
    // answers its 404 page — `RequireEmojiName`'s 400 is unreachable through the router. Measured
    // on the running server; the first version of this test expected 400 for all three.
    let cases = [
        ("short", post.as_str(), EMOJI, 400),
        (me, "short", EMOJI, 400),
        (me, post.as_str(), "not an emoji", 404),
        (me, post.as_str(), "a/b", 404),
    ];

    for (user_id, post_id, emoji, expected_status) in cases {
        // A 404 is Go's router refusing the path, which this server reproduces by forwarding —
        // so those responses legitimately carry `x-mmrs-served-by: go`.
        let expect_rust = expected_status != 404;
        let (go_status, go_body) =
            unreact_maybe_forwarded(&http, GO, &token, user_id, post_id, emoji, expect_rust).await;
        let (rust_status, rust_body) =
            unreact_maybe_forwarded(&http, RUST, &token, user_id, post_id, emoji, expect_rust)
                .await;
        assert_eq!(
            go_status, expected_status,
            "Go answered unexpectedly for {user_id}/{post_id}/{emoji}: {go_body}"
        );
        assert_eq!(
            rust_status, go_status,
            "the status differs for {user_id}/{post_id}/{emoji}"
        );
        // The 404 is compared **byte for byte** — its `detailed_error` quotes the request URL,
        // and reproducing that is exactly what forwarding buys. The 400 is ours, so `message`
        // carries the untranslated id and is exempted like every other refusal in this suite.
        let go_body = strip_request_id(&go_body);
        let rust_body = strip_request_id(&rust_body);
        if expected_status == 404 {
            assert_eq!(
                rust_body, go_body,
                "the forwarded 404 body differs for {user_id}/{post_id}/{emoji}"
            );
        } else {
            for field in ["id", "detailed_error", "status_code"] {
                assert_eq!(
                    rust_body[field], go_body[field],
                    "{field} differs for {user_id}/{post_id}/{emoji}"
                );
            }
        }
    }
}

/// `request_id` is per-request and never matches between two servers.
fn strip_request_id(raw: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(raw).expect("an AppError");
    if let Some(object) = value.as_object_mut() {
        object.remove("request_id");
    }
    value
}

#[tokio::test]
async fn an_archived_channel_refuses_both_directions() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel = create_channel_typed(&http, &admin, &team, "reactarch", "P").await;
    let post = post_message(&http, &admin, &channel, "mmrs reaction archived", None).await;
    let me = logged_in_user_id();

    // React first, so the delete path has something to refuse to remove.
    react(&http, GO, &admin, me, &post, EMOJI).await;
    delete_channel(&http, &admin, &channel).await;

    // **403, not 400** — the archived-channel arm sits directly below the restricted-DM arm,
    // which is a 400, and the two are one line apart in the Go source.
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &admin, me, &post, "+1").await;
        assert_eq!(status, 403, "{base} on a reaction to an archived channel");
        assert_eq!(body["id"], "api.reaction.save.archived_channel.app_error");
    }
    for base in [GO, RUST] {
        let (status, body) = unreact(&http, base, &admin, me, &post, EMOJI).await;
        assert_eq!(status, 403, "{base} on a delete in an archived channel");
        let body: serde_json::Value = serde_json::from_str(&body).expect("an AppError");
        assert_eq!(body["id"], "api.reaction.delete.archived_channel.app_error");
    }
}

// -------------------------------------------------------------------------------------------
// The seven mutations that survived the first run
//
// Each test below exists because a specific mutation of the finished code passed the suite. They
// are grouped here rather than woven into the tests above so that the reason each one exists
// stays legible.
// -------------------------------------------------------------------------------------------

/// Fifty system emoji, which is `ServiceSettingsDefaultUniqueReactionsPerPost`. Written out rather
/// than generated: every one has to be a real system emoji or the save is refused by the *emoji*
/// check before it reaches the limit, and a generated name would silently test the wrong branch.
const FIFTY_EMOJI: [&str; 50] = [
    "smile",
    "grinning",
    "joy",
    "heart",
    "thumbsup",
    "thumbsdown",
    "clap",
    "fire",
    "tada",
    "eyes",
    "rocket",
    "wave",
    "pray",
    "muscle",
    "ok_hand",
    "point_up",
    "raised_hands",
    "sunglasses",
    "thinking_face",
    "cry",
    "sob",
    "rage",
    "angry",
    "confused",
    "worried",
    "sleeping",
    "zzz",
    "star",
    "star2",
    "sparkles",
    "zap",
    "boom",
    "sunny",
    "cloud",
    "snowflake",
    "umbrella",
    "coffee",
    "beer",
    "cake",
    "pizza",
    "apple",
    "banana",
    "cherries",
    "grapes",
    "lemon",
    "melon",
    "peach",
    "pear",
    "strawberry",
    "tomato",
];

#[tokio::test]
async fn the_unique_emoji_limit_counts_distinct_undeleted_emoji() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    // **A channel of its own**, not the shared one `a_team_and_channel_the_user_is_in` returns.
    // That helper picks the *first* channel the admin is in, which is whatever some other suite
    // created most recently — and when that suite's fixtures are swept, this test's post goes with
    // them. Measured: the fiftieth `react` came back `app.post.get.app_error` 404, which reads as
    // an off-by-one in the limit and is not one. Fifty-one reactions is a long test; it needs a
    // channel nothing else touches.
    let team = create_team(&http, &token, "reactlimit").await;
    let channel = create_channel_typed(&http, &token, &team, "reactlimit", "O").await;
    let post = post_message(&http, &token, &channel, "mmrs reaction limit", None).await;
    let me = logged_in_user_id();

    // The order matters. `withdrawn` goes on **first** and is then removed, so by the time the
    // post is at the limit there is a soft-deleted row for an emoji that is not in the count.
    // Re-adding it at the limit is what separates `exists_on_post`'s `COALESCE(DeleteAt, 0) = 0`
    // from a bare existence check: with the filter it is a new distinct emoji and is refused;
    // without it, the limit is skipped and it is accepted.
    let withdrawn = "handshake";
    let (status, _, _) = react(&http, RUST, &token, me, &post, withdrawn).await;
    assert_eq!(status, 200, "the withdrawn emoji goes on first");
    let (status, body) = unreact(&http, RUST, &token, me, &post, withdrawn).await;
    assert_eq!(status, 200, "the withdrawal must succeed: {body}");

    // **The precondition, checked rather than assumed.** Everything below counts up to exactly
    // fifty distinct emoji, so the post must start with none. Left implicit, a withdrawal that
    // silently did not take makes the *fiftieth* emoji the fifty-first and the failure reads as
    // "the limit is off by one" — which is what this looked like the two times it flaked.
    let before = reactions_on(&http, RUST, &token, &post).await;
    assert!(
        before.is_empty(),
        "the post starts with no live reactions: {before:?}"
    );

    for emoji in FIFTY_EMOJI {
        let (status, body, _) = react(&http, RUST, &token, me, &post, emoji).await;
        assert_eq!(
            status, 200,
            "{emoji} should be accepted below the limit: {body}"
        );
    }

    // The 51st *distinct* emoji, on both servers, is refused with the same id and status.
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &token, me, &post, "beers").await;
        assert_eq!(status, 400, "{base} should refuse the 51st distinct emoji");
        assert_eq!(body["id"], "app.reaction.save.save.too_many_reactions");
    }

    // The soft-deleted emoji is a *new* distinct emoji, so it is refused too.
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &token, me, &post, withdrawn).await;
        assert_eq!(
            status, 400,
            "{base} should count a withdrawn emoji as absent, and refuse it at the limit"
        );
        assert_eq!(body["id"], "app.reaction.save.save.too_many_reactions");
    }

    // But an emoji **already on the post** skips the limit entirely — Go checks `ExistsOnPost`
    // before it counts. Re-reacting with one of the fifty must succeed at the limit.
    let (status, body, _) = react(&http, RUST, &token, me, &post, FIFTY_EMOJI[0]).await;
    assert_eq!(
        status, 200,
        "an emoji already on the post is exempt from the limit: {body}"
    );

    // Withdrawing one frees a slot, which proves the count filters on `DeleteAt`.
    unreact(&http, RUST, &token, me, &post, FIFTY_EMOJI[0]).await;
    let (status, body, _) = react(&http, RUST, &token, me, &post, "beers").await;
    assert_eq!(
        status, 200,
        "withdrawing one of the fifty makes room for a new distinct emoji: {body}"
    );
}

#[tokio::test]
async fn an_uppercase_emoji_name_is_lowercased_before_it_is_stored() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_post = post_message(&http, &token, &channel, "mmrs reaction case go", None).await;
    let rust_post = post_message(&http, &token, &channel, "mmrs reaction case rust", None).await;
    let me = logged_in_user_id();

    // Go lower-cases **before** validating, so `SMILE` is not an unknown emoji — it is `smile`.
    // A port that validated first would answer `app.emoji.get_by_name.no_result` here.
    for (base, post) in [(GO, &go_post), (RUST, &rust_post)] {
        let (status, body, _) = react(&http, base, &token, me, post, "SMILE").await;
        assert_eq!(
            status, 200,
            "{base} rejected an uppercase emoji name: {body}"
        );
        assert_eq!(
            body["emoji_name"], "smile",
            "{base} did not lower-case the emoji name"
        );
    }

    // **Go's lowercase is the *simple* per-rune mapping, and Rust's is not.**
    //
    // `strings.ToLower("İ")` is one byte, `i`. `str::to_lowercase` produces `i` + U+0307, three
    // bytes. Twenty-two of them are 44 bytes to Go and 66 to Rust — over the 64-byte cap the
    // handler applies *after* lowering, so the two servers answer different refusals for the same
    // request: Go reaches the emoji lookup and 404s, a naive port stops at its own 400.
    //
    // Measured on the running server. The port used `str::to_lowercase` until a surviving
    // mutation of the handler's lowercase led here; `mm_model::utils::go_to_lower` already
    // existed, pinned against a Go corpus, and its doc comment names this case exactly.
    let dotted = "İ".repeat(22);
    assert_eq!(dotted.len(), 44, "the fixture is 44 bytes before lowering");
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &token, me, &go_post, &dotted).await;
        assert_eq!(
            status, 404,
            "{base} should lower 44 bytes to 22 and reach the emoji lookup: {body}"
        );
        assert_eq!(
            body["id"], "app.emoji.get_by_name.no_result",
            "{base} refused before the emoji lookup, so its lowercase grew the name"
        );
    }

    // The mirror image, which pins the *order*: `Ⱥ` (U+023A, 2 bytes) lowers to `ⱥ` (U+2C65,
    // **3** bytes), so twenty-two of them grow from 44 bytes to 66 and fail the 64-byte cap —
    // but only because the cap is applied *after* lowering. A handler that lowered later would
    // let 44 bytes through and answer the emoji lookup's 404 instead. Measured: Go answers 400.
    let growing = "Ⱥ".repeat(22);
    assert_eq!(growing.len(), 44, "the fixture is 44 bytes before lowering");
    for base in [GO, RUST] {
        let (status, body, _) = react(&http, base, &token, me, &go_post, &growing).await;
        assert_eq!(
            status, 400,
            "{base} applies the length cap after lowering: {body}"
        );
        assert_eq!(
            body["id"], "api.reaction.save_reaction.invalid.app_error",
            "{base} did not apply the cap to the lowered name"
        );
    }

    // And the delete path lower-cases too, so the upper-cased name removes the lower-cased row.
    let (status, _) = unreact(&http, RUST, &token, me, &rust_post, "SMILE").await;
    assert_eq!(status, 200);
    assert!(
        reactions_on(&http, RUST, &token, &rust_post)
            .await
            .is_empty(),
        "the upper-cased delete did not remove the lower-cased row"
    );
    unreact(&http, GO, &token, me, &go_post, "SMILE").await;
}

#[tokio::test]
async fn an_emoji_name_with_punctuation_survives_the_routers_class() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction punctuation", None).await;
    let me = logged_in_user_id();

    // `+1` is a system emoji whose name is outside `[A-Za-z0-9]` — the class the *ids* use. The
    // emoji segment's class adds `_`, `-` and `+`, and a port that reused the id class here would
    // forward this delete to Go and answer its 404.
    for emoji in ["+1", "-1", "ok_hand"] {
        let (status, body, _) = react(&http, RUST, &token, me, &post, emoji).await;
        assert_eq!(status, 200, "{emoji} should be a valid reaction: {body}");
        let (status, body) = unreact(&http, RUST, &token, me, &post, emoji).await;
        assert_eq!(
            status, 200,
            "{emoji} should be deletable without forwarding: {body}"
        );
    }
    assert!(reactions_on(&http, RUST, &token, &post).await.is_empty());
}

#[tokio::test]
async fn has_reactions_survives_removing_one_of_two() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction hasreactions", None).await;
    let me = logged_in_user_id();

    // `Posts.HasReactions` is on the wire, and the delete path **recomputes** it from the
    // remaining undeleted reactions rather than clearing it. Removing one of two must leave it
    // set; a port that wrote `FALSE` would clear the reaction bar for the surviving reaction.
    react(&http, RUST, &token, me, &post, "smile").await;
    react(&http, RUST, &token, me, &post, "tada").await;
    assert!(
        has_reactions(&http, RUST, &token, &post).await,
        "two reactions should set has_reactions"
    );

    unreact(&http, RUST, &token, me, &post, "smile").await;
    assert!(
        has_reactions(&http, RUST, &token, &post).await,
        "one reaction remains, so has_reactions must stay set"
    );

    unreact(&http, RUST, &token, me, &post, "tada").await;
    assert!(
        !has_reactions(&http, RUST, &token, &post).await,
        "the last reaction going should clear has_reactions"
    );
}

async fn has_reactions(http: &reqwest::Client, base: &str, token: &str, post_id: &str) -> bool {
    let response = http
        .get(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("a readable post");
    let post: serde_json::Value = response.json().await.expect("a post");
    post["is_pinned"].is_boolean() && post["has_reactions"].as_bool().unwrap_or(false)
}

#[tokio::test]
async fn the_authors_own_reaction_to_a_persistent_notification_post_is_served_here() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "mmrs reaction persistent", None).await;
    let me = logged_in_user_id();

    // A persistent-notification post cannot be created through the REST API without the priority
    // metadata and a licence, so the row is planted. Silent without a `DATABASE_URL`.
    if !plant_persistent_notification(&post).await {
        return;
    }

    // **The author is exempt** — `ResolvePersistentNotification`'s first line — so this reaction
    // is decidable here and must not be forwarded. Dropping that exemption makes the same request
    // forward, which is invisible in the body and visible in the header.
    let response = http
        .post(format!("{RUST}/api/v4/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"user_id": me, "post_id": post, "emoji_name": EMOJI}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "the post's own author is exempt, so this must not forward"
    );

    unreact(&http, RUST, &token, me, &post, EMOJI).await;
    clear_persistent_notification(&post).await;
}

/// Write a live `PersistentNotifications` row for `post_id`.
///
/// No REST route creates one without post priority and a licence, and the row is the only thing
/// that makes `ResolvePersistentNotification` undecidable here. Returns false when the suite is
/// running without a `DATABASE_URL`.
async fn plant_persistent_notification(post_id: &str) -> bool {
    let Some(pool) = test_pool().await else {
        return false;
    };
    sqlx::query(
        "INSERT INTO persistentnotifications (postid, createat, lastsentat, deleteat, sentcount) \
         VALUES ($1, $2, 0, 0, 0) \
         ON CONFLICT (postid) DO UPDATE SET deleteat = 0",
    )
    .bind(post_id)
    .bind(chrono::Utc::now().timestamp_millis())
    .execute(&pool)
    .await
    .expect("the row is written");
    true
}

async fn clear_persistent_notification(post_id: &str) {
    let Some(pool) = test_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM persistentnotifications WHERE postid = $1")
        .bind(post_id)
        .execute(&pool)
        .await;
}

async fn test_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

#[tokio::test]
async fn add_reaction_and_remove_reaction_are_different_permissions() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;

    // **`purge_api_fixtures` before `plant_role`, not after.** The purge sweeps `mmrs_role_%`
    // rows, and `create_plain_user` calls it — so planting first and creating the user second
    // deletes the role between planting it and using it. The session still names the role, Go
    // finds no row, and every permission it granted silently disappears: both halves of this test
    // answered 403 until the order was fixed. The purge is a `OnceCell`, so calling it here makes
    // the one inside `create_plain_user` a no-op.
    common::purge_api_fixtures().await;

    // **No stock role separates `add_reaction` from `remove_reaction`** — `channel_user` holds
    // both — so a mutation swapping one for the other is invisible to any fixture built from
    // stock roles. It survived the first run. A planted role holding *only* the remove side, on a
    // user who is not a channel member, is what tells them apart.
    let Some(role) = common::plant_role(
        "reactremove",
        "remove_reaction remove_others_reactions read_channel",
    )
    .await
    else {
        return; // no DATABASE_URL
    };
    let plain = create_plain_user(&http, &admin, &team, "reactremove").await;
    common::set_user_roles(&plain.id, &format!("system_user {role}")).await;
    let plain_token = login_plain_user(&http, "reactremove").await;

    let post = post_message(&http, &admin, &channel, "mmrs reaction perms", None).await;
    react(&http, GO, &admin, logged_in_user_id(), &post, EMOJI).await;

    // The remove side is granted, so the delete succeeds. Under the mutation this handler asks
    // for `add_reaction`, which this role does not hold, and the same request is a 403.
    let (go_status, go_body) =
        unreact(&http, GO, &plain_token, logged_in_user_id(), &post, EMOJI).await;
    assert_eq!(
        go_status, 200,
        "Go allows a remove-only role to withdraw somebody else's reaction: {go_body}"
    );

    // And the add side is refused for the same session, which is the other half of the claim.
    let (status, body, _) = react(&http, RUST, &plain_token, &plain.id, &post, EMOJI).await;
    assert_eq!(
        status, 403,
        "a remove-only role must not be able to add a reaction: {body}"
    );

    // Now the same delete through us, on a fresh reaction.
    react(&http, GO, &admin, logged_in_user_id(), &post, EMOJI).await;
    let (rust_status, body) =
        unreact(&http, RUST, &plain_token, logged_in_user_id(), &post, EMOJI).await;
    assert_eq!(
        rust_status, go_status,
        "we disagree with Go about the remove-only role: {body}"
    );

    delete_plain_user(&http, &admin, &plain.id).await;
}
