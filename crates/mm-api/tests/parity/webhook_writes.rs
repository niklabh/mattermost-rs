//! Cross-server parity for the seven webhook writes.
//!
//! Pure CRUD with no websocket events, which makes the *permission ladders* the interesting part:
//! four checks on the incoming create, two of them team-scoped and one that is **not a refusal at
//! all** — without `bypass_incoming_webhook_channel_lock` the hook is silently forced closed.
//!
//! Reads back through the server that wrote, per [D-190].
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity webhook_writes
//! ```

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, client, create_channel_typed, go_minted_token,
    logged_in_user_id, stack_enabled,
};

async fn post_hook(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

async fn put_hook(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

async fn delete_hook(http: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, String) {
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

async fn get_hook(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, serde_json::Value) {
    let response = http
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("a readable hook");
    let status = response.status().as_u16();
    let value = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Replace the id, token and timestamps, keeping only whether each is set — everything else has
/// to match between the two servers.
fn normalise(hook: &serde_json::Value) -> serde_json::Value {
    let mut out = hook.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for key in [
        "id",
        "token",
        "channel_id",
        "team_id",
        "user_id",
        "creator_id",
    ] {
        if let Some(value) = object.get(key) {
            let present = value.as_str().is_some_and(|s| !s.is_empty());
            object.insert(key.to_owned(), serde_json::json!(present));
        }
    }
    for key in ["create_at", "update_at"] {
        if let Some(value) = object.get(key) {
            let nonzero = value.as_i64().unwrap_or(0) > 0;
            object.insert(key.to_owned(), serde_json::json!(nonzero));
        }
    }
    out
}

#[tokio::test]
async fn an_incoming_hook_round_trips_and_both_servers_answer_the_same_shape() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "hookincg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "hookincr", "O").await;

    let body = |channel: &str, name: &str| {
        serde_json::json!({
            "channel_id": channel,
            "display_name": name,
            "description": "mmrs parity incoming hook",
        })
    };

    let (go_status, go_raw) = post_hook(
        &http,
        GO,
        &token,
        "/api/v4/hooks/incoming",
        &body(&go_channel, "mmrs go hook"),
    )
    .await;
    let (rust_status, rust_raw) = post_hook(
        &http,
        RUST,
        &token,
        "/api/v4/hooks/incoming",
        &body(&rust_channel, "mmrs rust hook"),
    )
    .await;

    assert_eq!(go_status, 201, "Go answers Created: {go_raw}");
    assert_eq!(rust_status, go_status, "the create status differs");
    assert!(
        go_raw.ends_with('\n'),
        "Go's create body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(rust_raw.ends_with('\n'), go_raw.ends_with('\n'));

    let go_hook: serde_json::Value = serde_json::from_str(&go_raw).expect("a hook");
    let rust_hook: serde_json::Value = serde_json::from_str(&rust_raw).expect("a hook");
    let mut go_normalised = normalise(&go_hook);
    let mut rust_normalised = normalise(&rust_hook);
    go_normalised["display_name"] = serde_json::json!("<name>");
    rust_normalised["display_name"] = serde_json::json!("<name>");
    assert_eq!(
        go_normalised, rust_normalised,
        "the created hook differs:\n go: {go_hook}\nrust: {rust_hook}"
    );

    // **`channel_locked` is forced true.** The admin holds
    // `bypass_incoming_webhook_channel_lock`… or does not; whatever the answer, both servers must
    // agree, which is what this asserts. It is not a refusal either way.
    assert_eq!(
        rust_hook["channel_locked"], go_hook["channel_locked"],
        "the two servers disagree about the channel lock"
    );

    let rust_id = rust_hook["id"].as_str().expect("an id").to_owned();
    let go_id = go_hook["id"].as_str().expect("an id").to_owned();

    // Read back through the writing server.
    let (status, ours) = get_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{rust_id}"),
    )
    .await;
    assert_eq!(status, 200, "our hook reads back: {ours}");
    assert_eq!(ours["display_name"], "mmrs rust hook");

    // Update: a `PUT` that answers **201**, and `user_id` cannot be changed by the body.
    let update = serde_json::json!({
        "id": rust_id,
        "channel_id": rust_channel,
        "display_name": "mmrs rust hook renamed",
        "description": "edited",
        "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
    });
    let (status, raw) = put_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{rust_id}"),
        &update,
    )
    .await;
    assert_eq!(status, 201, "a PUT that answers Created: {raw}");
    let updated: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    assert_eq!(updated["display_name"], "mmrs rust hook renamed");
    assert_eq!(
        updated["user_id"], rust_hook["user_id"],
        "user_id is copied off the old hook, not taken from the body"
    );
    assert_eq!(
        updated["create_at"], rust_hook["create_at"],
        "create_at is copied off the old hook"
    );

    // **Read the row back, not the answer.** The response is marshalled from the struct the app
    // layer assembled, so it is identical whatever the `UPDATE` actually wrote — a mutation that
    // swapped `displayname` and `description` in the store's `SET` list survived until this read
    // existed. Through us, because Go's webhook cache cannot see our write ([D-190]).
    let (status, stored) = get_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{rust_id}"),
    )
    .await;
    assert_eq!(status, 200, "the updated hook reads back: {stored}");
    assert_eq!(
        stored["display_name"], "mmrs rust hook renamed",
        "the stored display_name is what the update named"
    );
    assert_eq!(
        stored["description"], "edited",
        "and the description landed in its own column"
    );
    assert_eq!(
        stored["user_id"], rust_hook["user_id"],
        "the stored owner is unchanged"
    );

    // Go's update answers 201 too — asserted on its own hook.
    let go_update = serde_json::json!({
        "id": go_id,
        "channel_id": go_channel,
        "display_name": "mmrs go hook renamed",
    });
    let (go_status, _) = put_hook(
        &http,
        GO,
        &token,
        &format!("/api/v4/hooks/incoming/{go_id}"),
        &go_update,
    )
    .await;
    assert_eq!(go_status, 201, "Go's PUT answers Created too");

    // Delete is a soft delete, so the single read stops finding it — a 404, not a deleted row.
    let (status, body) = delete_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{rust_id}"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"OK"}"#);
    let (status, _) = get_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{rust_id}"),
    )
    .await;
    assert_eq!(
        status, 404,
        "a soft-deleted hook is not found, not returned"
    );

    delete_hook(
        &http,
        GO,
        &token,
        &format!("/api/v4/hooks/incoming/{go_id}"),
    )
    .await;
    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn an_outgoing_hook_round_trips_and_regenerates_its_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;

    // Trigger words have to be unique per team across *every* hook, including deleted ones — the
    // intersection scan carries no `DeleteAt` predicate — so each run needs its own words.
    let tag = format!("mmrsout{}", logged_in_user_id().get(..6).unwrap_or("x"));
    let body = |suffix: &str| {
        serde_json::json!({
            "team_id": team,
            "display_name": format!("mmrs outgoing {suffix}"),
            "trigger_words": [format!("{tag}{suffix}")],
            "callback_urls": [format!("https://example.invalid/{tag}{suffix}")],
        })
    };

    let (go_status, go_raw) =
        post_hook(&http, GO, &token, "/api/v4/hooks/outgoing", &body("go")).await;
    let (rust_status, rust_raw) =
        post_hook(&http, RUST, &token, "/api/v4/hooks/outgoing", &body("rs")).await;

    assert_eq!(go_status, 201, "Go answers Created: {go_raw}");
    assert_eq!(rust_status, go_status, "the create status differs");

    let go_hook: serde_json::Value = serde_json::from_str(&go_raw).expect("a hook");
    let rust_hook: serde_json::Value = serde_json::from_str(&rust_raw).expect("a hook");
    let mut go_normalised = normalise(&go_hook);
    let mut rust_normalised = normalise(&rust_hook);
    for value in [&mut go_normalised, &mut rust_normalised] {
        value["display_name"] = serde_json::json!("<name>");
        value["trigger_words"] = serde_json::json!("<words>");
        value["callback_urls"] = serde_json::json!("<urls>");
    }
    assert_eq!(
        go_normalised, rust_normalised,
        "the created hook differs:\n go: {go_hook}\nrust: {rust_hook}"
    );
    assert_eq!(
        rust_hook["creator_id"],
        logged_in_user_id(),
        "creator_id is filled from the session when the body omits it"
    );

    let rust_id = rust_hook["id"].as_str().expect("an id").to_owned();
    let go_id = go_hook["id"].as_str().expect("an id").to_owned();

    // **The update answers 200**, where the incoming update answers 201.
    let update = serde_json::json!({
        "id": rust_id,
        "team_id": team,
        "display_name": "mmrs outgoing renamed",
        "trigger_words": [format!("{tag}rs")],
        "callback_urls": [format!("https://example.invalid/{tag}rs")],
    });
    let (status, raw) = put_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{rust_id}"),
        &update,
    )
    .await;
    assert_eq!(
        status, 200,
        "the outgoing update answers OK, not Created: {raw}"
    );
    let updated: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    assert_eq!(updated["display_name"], "mmrs outgoing renamed");

    // **An update that omits `token` blanks it**, on both servers. `UpdateOutgoingWebhook` copies
    // `CreatorId`, `CreateAt`, `DeleteAt`, `TeamId` and `UpdateAt` off the old hook — and *not*
    // `Token` — while the store's `SET` list writes `Token` along with everything else. So a
    // client that edits a hook's display name without echoing the token back destroys the
    // integration's credential.
    //
    // Measured against the running Go server before it was asserted here. Reproduced rather than
    // repaired: a port that preserved the token would answer a value Go does not have.
    assert_eq!(
        updated["token"], "",
        "an update that omits the token blanks it, as Go's does"
    );

    // Regenerating gives it a token back — which is the only way to recover from the blanking
    // above, and probably why the route exists.
    let (status, raw) = post_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{rust_id}/regen_token"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, 200, "regen answers OK: {raw}");
    let regenerated: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    assert_ne!(
        regenerated["token"], updated["token"],
        "the token must change"
    );
    assert_eq!(
        regenerated["token"].as_str().map(str::len),
        Some(26),
        "and it is a freshly minted id"
    );
    assert_eq!(
        regenerated["display_name"], updated["display_name"],
        "and nothing else does"
    );
    assert_eq!(
        regenerated["create_at"], updated["create_at"],
        "regen does not touch create_at"
    );

    // And Go agrees about the regen's status and shape, on its own hook.
    let (go_status, go_regen_raw) = post_hook(
        &http,
        GO,
        &token,
        &format!("/api/v4/hooks/outgoing/{go_id}/regen_token"),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(go_status, 200);
    assert!(
        go_regen_raw.ends_with('\n'),
        "Go's regen body is encoder-framed: {go_regen_raw:?}"
    );
    assert_eq!(raw.ends_with('\n'), go_regen_raw.ends_with('\n'));

    delete_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{rust_id}"),
    )
    .await;
    delete_hook(
        &http,
        GO,
        &token,
        &format!("/api/v4/hooks/outgoing/{go_id}"),
    )
    .await;
}

#[tokio::test]
async fn an_outgoing_hook_needs_a_channel_or_trigger_words() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let private = create_channel_typed(&http, &token, &team, "hookpriv", "P").await;

    // No channel and no trigger words: **400** on create.
    //
    // Both spellings of "no trigger words" are tested. The field **absent** and the field present
    // as **`[]`** take different paths through `trigger_words.as_ref().is_none_or(...)`: the first
    // short-circuits on the `None` and never reaches the emptiness test. A mutation replacing that
    // test with `false` survived a fixture that only omitted the key — the absent case is refused
    // either way.
    let bare = serde_json::json!({
        "team_id": team,
        "display_name": "mmrs outgoing bare",
        "callback_urls": ["https://example.invalid/bare"],
    });
    let empty_words = serde_json::json!({
        "team_id": team,
        "display_name": "mmrs outgoing empty words",
        "trigger_words": [],
        "callback_urls": ["https://example.invalid/emptywords"],
    });
    for (what, body) in [("absent", &bare), ("empty", &empty_words)] {
        for base in [GO, RUST] {
            let (status, raw) =
                post_hook(&http, base, &token, "/api/v4/hooks/outgoing", body).await;
            assert_eq!(
                status, 400,
                "{base} should refuse a hook with {what} trigger words: {raw}"
            );
            let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
            assert_eq!(body["id"], "api.webhook.create_outgoing.triggers.app_error");
        }
    }

    // A **private** channel is refused with `api.outgoing_webhook.disabled.app_error` at 403 —
    // the same id the feature gate uses at 501. One id, two meanings, in one function.
    let in_private = serde_json::json!({
        "team_id": team,
        "channel_id": private,
        "display_name": "mmrs outgoing private",
        "trigger_words": ["mmrsprivtrigger"],
        "callback_urls": ["https://example.invalid/priv"],
    });
    for base in [GO, RUST] {
        let (status, raw) =
            post_hook(&http, base, &token, "/api/v4/hooks/outgoing", &in_private).await;
        assert_eq!(status, 403, "{base} should refuse a private channel: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "api.outgoing_webhook.disabled.app_error",
            "{base} used a different id for the non-open channel"
        );
    }

    common::delete_channel(&http, &token, &private).await;
}

#[tokio::test]
async fn two_outgoing_hooks_may_not_share_a_trigger_and_a_callback() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;

    let word = "mmrsintersecttrigger";
    let url = "https://example.invalid/mmrsintersect";
    let first = serde_json::json!({
        "team_id": team,
        "display_name": "mmrs intersect first",
        "trigger_words": [word],
        "callback_urls": [url],
    });

    let (status, raw) = post_hook(&http, RUST, &token, "/api/v4/hooks/outgoing", &first).await;
    assert_eq!(status, 201, "the first hook is created: {raw}");
    let created: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    let id = created["id"].as_str().expect("an id").to_owned();

    // **All three must match**: same channel (both empty), a shared callback, and a shared
    // trigger. The create path's collision is a **500**, which is Go's, and the update path's is a
    // 400 for the same condition.
    for base in [GO, RUST] {
        let (status, raw) = post_hook(&http, base, &token, "/api/v4/hooks/outgoing", &first).await;
        assert_eq!(status, 500, "{base} should refuse the collision: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"],
            "api.webhook.create_outgoing.intersect.app_error"
        );
    }

    // **Two hooks in different channels never collide**, however much they share — the check's
    // first test is the channel, and a mutation dropping it survived until this fixture existed.
    let channel_a = create_channel_typed(&http, &token, &team, "hookisecta", "O").await;
    let channel_b = create_channel_typed(&http, &token, &team, "hookisectb", "O").await;
    let scoped = |channel: &str, name: &str| {
        serde_json::json!({
            "team_id": team,
            "channel_id": channel,
            "display_name": format!("mmrs intersect {name}"),
            "trigger_words": ["mmrsscopedtrigger"],
            "callback_urls": ["https://example.invalid/mmrsscoped"],
        })
    };
    let (status, raw) = post_hook(
        &http,
        RUST,
        &token,
        "/api/v4/hooks/outgoing",
        &scoped(&channel_a, "a"),
    )
    .await;
    assert_eq!(status, 201, "the first scoped hook is created: {raw}");
    let a: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    let a_id = a["id"].as_str().expect("an id").to_owned();

    let (status, raw) = post_hook(
        &http,
        RUST,
        &token,
        "/api/v4/hooks/outgoing",
        &scoped(&channel_b, "b"),
    )
    .await;
    assert_eq!(
        status, 201,
        "identical triggers and callbacks in a *different* channel must be allowed: {raw}"
    );
    let b: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    let b_id = b["id"].as_str().expect("an id").to_owned();

    // And in the *same* channel they do collide.
    let (status, raw) = post_hook(
        &http,
        RUST,
        &token,
        "/api/v4/hooks/outgoing",
        &scoped(&channel_a, "again"),
    )
    .await;
    assert_eq!(
        status, 500,
        "the same channel, trigger and callback is a collision: {raw}"
    );

    delete_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{a_id}"),
    )
    .await;
    delete_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{b_id}"),
    )
    .await;
    common::delete_channel(&http, &token, &channel_a).await;
    common::delete_channel(&http, &token, &channel_b).await;

    // A hook sharing only the trigger word is fine.
    let trigger_only = serde_json::json!({
        "team_id": team,
        "display_name": "mmrs intersect trigger only",
        "trigger_words": [word],
        "callback_urls": ["https://example.invalid/mmrsdifferent"],
    });
    let (status, raw) =
        post_hook(&http, RUST, &token, "/api/v4/hooks/outgoing", &trigger_only).await;
    assert_eq!(
        status, 201,
        "sharing only the trigger word is not a collision: {raw}"
    );
    let second: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    let second_id = second["id"].as_str().expect("an id").to_owned();

    // And updating a hook to its own values does **not** collide with itself.
    let same = serde_json::json!({
        "id": id,
        "team_id": team,
        "display_name": "mmrs intersect first",
        "trigger_words": [word],
        "callback_urls": [url],
    });
    let (status, raw) = put_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{id}"),
        &same,
    )
    .await;
    assert_eq!(
        status, 200,
        "a hook must not collide with itself on update: {raw}"
    );

    delete_hook(&http, RUST, &token, &format!("/api/v4/hooks/outgoing/{id}")).await;
    delete_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/outgoing/{second_id}"),
    )
    .await;
}

#[tokio::test]
async fn the_id_checks_and_the_team_mismatch_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _first) = a_team_and_channel_the_user_is_in(&http, &token).await;
    // **Not the helper's channel.** `a_team_and_channel_the_user_is_in` returns the *first*
    // channel of the first team, which on this fixture is a **direct message** — type `D`, with
    // an empty `team_id`. Every webhook permission here is team-scoped, so a DM channel is a 403
    // on both servers, which is agreement about the wrong thing.
    let channel = create_channel_typed(&http, &token, &team, "hookids", "O").await;
    let other_team = common::create_team(&http, &token, "hookteam").await;

    // A malformed hook id in the path.
    for base in [GO, RUST] {
        let (status, raw) = delete_hook(&http, base, &token, "/api/v4/hooks/incoming/short").await;
        assert_eq!(status, 400, "{base} on a malformed hook id: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }

    // A body whose id disagrees with the path.
    let mismatched = serde_json::json!({
        "id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "channel_id": channel,
        "display_name": "mmrs mismatch",
    });
    for base in [GO, RUST] {
        let (status, raw) = put_hook(
            &http,
            base,
            &token,
            "/api/v4/hooks/incoming/bbbbbbbbbbbbbbbbbbbbbbbbbb",
            &mismatched,
        )
        .await;
        assert_eq!(status, 400, "{base} on an id mismatch: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // The team mismatch: a real hook, updated with a different team in the body.
    let (status, raw) = post_hook(
        &http,
        RUST,
        &token,
        "/api/v4/hooks/incoming",
        &serde_json::json!({"channel_id": channel, "display_name": "mmrs team mismatch"}),
    )
    .await;
    assert_eq!(status, 201, "{raw}");
    let hook: serde_json::Value = serde_json::from_str(&raw).expect("a hook");
    let id = hook["id"].as_str().expect("an id").to_owned();

    let wrong_team = serde_json::json!({
        "id": id,
        "channel_id": channel,
        "team_id": other_team,
        "display_name": "mmrs team mismatch",
    });
    for base in [GO, RUST] {
        let (status, raw) = put_hook(
            &http,
            base,
            &token,
            &format!("/api/v4/hooks/incoming/{id}"),
            &wrong_team,
        )
        .await;
        assert_eq!(status, 400, "{base} on a team mismatch: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "api.webhook.team_mismatch.app_error",
            "{base} used a different id for the team mismatch"
        );
    }

    // Omitting `team_id` entirely is allowed — it is filled from the old hook, then compared to
    // itself. That ordering is the whole reason the check can pass at all.
    let no_team = serde_json::json!({
        "id": id,
        "channel_id": channel,
        "display_name": "mmrs team omitted",
    });
    let (status, raw) = put_hook(
        &http,
        RUST,
        &token,
        &format!("/api/v4/hooks/incoming/{id}"),
        &no_team,
    )
    .await;
    assert_eq!(
        status, 201,
        "an omitted team_id is filled in, not rejected: {raw}"
    );

    delete_hook(&http, RUST, &token, &format!("/api/v4/hooks/incoming/{id}")).await;
    common::delete_channel(&http, &token, &channel).await;
}
