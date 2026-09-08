//! Cross-server parity for the four custom-status routes, which finish `api4/status.go`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity custom_status_writes
//! ```
//!
//! **A custom status is not a `Status` row.** It lives in `Users.Props["customStatus"]` as a JSON
//! string, so every write here is a whole `UpdateUser` — which is why these four were blocked on
//! that function and not on the status cache. The recents live somewhere else again, in a
//! `Preferences` row, so a single request touches two tables and publishes three websocket events
//! plus two more from `UpdatePreferences`.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, client, create_plain_user, delete_plain_user, go_minted_token,
    login_plain_user, stack_enabled,
};

fn custom_path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/status/custom")
}

fn recent_path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/status/custom/recent")
}

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// The `customStatus` prop as the row holds it — a JSON **string**, or absent.
async fn stored_custom_status(user_id: &str) -> Option<String> {
    let props = common::user_props(user_id).await?;
    props
        .get("customStatus")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

async fn recents(http: &reqwest::Client, base: &str, token: &str, user_id: &str) -> String {
    let path =
        format!("/api/v4/users/{user_id}/preferences/custom_status/name/recent_custom_statuses");
    let response = http
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the preference reads");
    if response.status() != 200 {
        return String::new();
    }
    let preference: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    preference["value"].as_str().unwrap_or_default().to_owned()
}

/// `user_updated` frames whose subject is `user_id` **and** whose custom status carries `text`.
///
/// Filtering by the subject alone is not enough, and that is not hypothetical: every test in this
/// file changes the *same* shared admin's status, so a sibling running concurrently publishes a
/// `user_updated` for exactly this user. Under qemu the tests were spread far enough apart to miss
/// each other; against the native server two of them started failing on every run. The status text
/// is unique per call site, so it is what identifies "my" event.
fn updates_for(
    events: Vec<serde_json::Value>,
    user_id: &str,
    text: &str,
) -> Vec<serde_json::Value> {
    events
        .into_iter()
        .filter(|event| {
            event["data"]["user"]["id"] == user_id
                && event["data"]["user"]["props"]["customStatus"]
                    .as_str()
                    .is_some_and(|status| status.contains(text))
        })
        .collect()
}

/// Wait — rather than sleep — for one such frame, then keep collecting briefly so that a *second*
/// one would also be seen. Both halves matter: the tests here assert a count of exactly one, so
/// arriving late and arriving twice are both failures worth catching.
async fn wait_for_update(socket: &mut SocketProbe, user_id: &str, text: &str) {
    let user_id = user_id.to_owned();
    let text = text.to_owned();
    socket
        .collect_until(Duration::from_secs(5), move |frames| {
            frames.iter().any(|frame| {
                frame["event"] == "user_updated"
                    && frame["data"]["user"]["id"] == user_id
                    && frame["data"]["user"]["props"]["customStatus"]
                        .as_str()
                        .is_some_and(|status| status.contains(&text))
            })
        })
        .await;
    socket.collect_for(Duration::from_millis(400)).await;
}

#[tokio::test]
async fn a_custom_status_round_trips_and_lands_in_the_user_row() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "csgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "csrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        let (code, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(&user.id),
            Some(&serde_json::json!({"emoji": "grinning", "text": "in a meeting"})),
        )
        .await;
        assert_eq!(code, 200, "{base} setting a custom status: {raw}");
        // `ReturnStatusOK` is `w.Write(MapToJSON(...))` — no trailing newline.
        assert_eq!(raw, r#"{"status":"OK"}"#, "{base} answered {raw:?}");

        // The prop is a JSON **string**, not a nested object.
        let stored = stored_custom_status(&user.id)
            .await
            .unwrap_or_else(|| panic!("{base} wrote no customStatus prop"));
        let parsed: serde_json::Value = serde_json::from_str(&stored).expect("the prop holds JSON");
        assert_eq!(parsed["emoji"], "grinning", "{base}: {stored}");
        assert_eq!(parsed["text"], "in a meeting", "{base}: {stored}");
    }

    // Both servers wrote the same shape, keys included — `duration` and `expires_at` are not
    // `omitempty` in Go, so an unset status still carries them.
    let go_keys: Vec<String> = serde_json::from_str::<serde_json::Value>(
        &stored_custom_status(&go_user.id).await.expect("go prop"),
    )
    .expect("JSON")
    .as_object()
    .expect("an object")
    .keys()
    .cloned()
    .collect();
    let rust_keys: Vec<String> = serde_json::from_str::<serde_json::Value>(
        &stored_custom_status(&rust_user.id)
            .await
            .expect("rust prop"),
    )
    .expect("JSON")
    .as_object()
    .expect("an object")
    .keys()
    .cloned()
    .collect();
    assert_eq!(go_keys, rust_keys, "the stored prop's keys differ");

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

/// `PreSave` does two things and both are observable in the stored prop.
#[tokio::test]
async fn pre_save_truncates_the_text_and_defaults_the_duration() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "cspsgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "cspsrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        // **`CustomStatusTextMaxRunes` is 100 runes, not 100 bytes.** A multi-byte character
        // makes the two differ, so the text is deliberately not ASCII: a byte-slicing port would
        // either cut short or panic on a boundary.
        let long_text: String = "é".repeat(150);
        let (code, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(&user.id),
            Some(&serde_json::json!({"emoji": "wave", "text": long_text})),
        )
        .await;
        assert_eq!(code, 200, "{base} setting a long text: {raw}");

        let stored = stored_custom_status(&user.id).await.expect("a prop");
        let parsed: serde_json::Value = serde_json::from_str(&stored).expect("JSON");
        let text = parsed["text"].as_str().expect("a text");
        assert_eq!(
            text.chars().count(),
            100,
            "{base} truncated to 100 **runes**: {} bytes",
            text.len()
        );
        assert_eq!(text, "é".repeat(100), "{base} cut on a rune boundary");

        // A future `expires_at` with **no** duration gets `date_and_time` filled in.
        let (code, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(&user.id),
            Some(&serde_json::json!({
                "emoji": "wave",
                "text": "expiring",
                "expires_at": "2099-01-01T00:00:00Z",
            })),
        )
        .await;
        assert_eq!(code, 200, "{base} setting an expiring status: {raw}");

        let stored = stored_custom_status(&user.id).await.expect("a prop");
        let parsed: serde_json::Value = serde_json::from_str(&stored).expect("JSON");
        assert_eq!(
            parsed["duration"], "date_and_time",
            "{base} defaulted the duration: {stored}"
        );
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

/// `ClearCustomStatus` writes the **empty string** rather than removing the key, so a cleared
/// status and a never-set one are different rows.
#[tokio::test]
async fn removing_a_custom_status_empties_the_prop_rather_than_dropping_it() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "csdelgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "csdelrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        // A user who never had one: the key is absent.
        assert_eq!(
            stored_custom_status(&user.id).await,
            None,
            "{base}: a fresh user has no customStatus key"
        );

        send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(&user.id),
            Some(&serde_json::json!({"emoji": "wave", "text": "hi"})),
        )
        .await;

        let (code, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &custom_path(&user.id),
            None,
        )
        .await;
        assert_eq!(code, 200, "{base} removing: {raw}");
        assert_eq!(raw, r#"{"status":"OK"}"#);

        assert_eq!(
            stored_custom_status(&user.id).await,
            Some(String::new()),
            "{base} left the key with an empty value, not absent"
        );
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

/// Setting a status appends to a **preference**, most recent first, capped at five, deduplicated
/// by `text`.
#[tokio::test]
async fn the_recents_are_a_preference_capped_at_five_and_deduplicated_by_text() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "csrecgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "csrecrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        for text in ["one", "two", "three", "four", "five", "six"] {
            let (code, raw) = send(
                &http,
                reqwest::Method::PUT,
                base,
                &admin,
                &custom_path(&user.id),
                Some(&serde_json::json!({"emoji": "wave", "text": text})),
            )
            .await;
            assert_eq!(code, 200, "{base} setting {text}: {raw}");
        }
        // And once more with a text already in the list — it moves to the head rather than
        // appearing twice.
        send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(&user.id),
            Some(&serde_json::json!({"emoji": "wave", "text": "four"})),
        )
        .await;

        // Read the recents back through the server that wrote them: Go caches preferences.
        let value = recents(&http, base, &admin, &user.id).await;
        let list: Vec<serde_json::Value> =
            serde_json::from_str(&value).unwrap_or_else(|e| panic!("{base}: {value:?}: {e}"));
        let texts: Vec<&str> = list
            .iter()
            .map(|cs| cs["text"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            texts,
            vec!["four", "six", "five", "three", "two"],
            "{base}: newest first, five at most, one entry per text"
        );
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

/// Both spellings of the recents delete, and its refusals.
#[tokio::test]
async fn the_two_recent_delete_paths_agree_with_each_other_and_across_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "csrdgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "csrdrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        for text in ["alpha", "beta"] {
            send(
                &http,
                reqwest::Method::PUT,
                base,
                &admin,
                &custom_path(&user.id),
                Some(&serde_json::json!({"emoji": "wave", "text": text})),
            )
            .await;
        }

        // **The membership test is a byte comparison of the marshalled status**, so the request
        // must name every field the stored entry has — an emoji-only guess is a 400.
        let stored = recents(&http, base, &admin, &user.id).await;
        let list: Vec<serde_json::Value> = serde_json::from_str(&stored).expect("a list");
        let alpha = list
            .iter()
            .find(|cs| cs["text"] == "alpha")
            .expect("alpha is in the list")
            .clone();

        // The DELETE form.
        let (code, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &recent_path(&user.id),
            Some(&alpha),
        )
        .await;
        assert_eq!(code, 200, "{base} deleting a recent: {raw}");
        assert_eq!(raw, r#"{"status":"OK"}"#);

        let after = recents(&http, base, &admin, &user.id).await;
        assert!(
            !after.contains("alpha"),
            "{base} did not remove it: {after}"
        );
        assert!(after.contains("beta"), "{base} removed too much: {after}");

        // Deleting it again is a 400 — `Contains` is false, and that is one of the three
        // failures sharing `recent_custom_statuses.delete.app_error`.
        let (code, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &recent_path(&user.id),
            Some(&alpha),
        )
        .await;
        assert_eq!(code, 400, "{base} on a second delete: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "api.custom_status.recent_custom_statuses.delete.app_error",
            "{base}"
        );

        // The POST form removes the other one, proving the two registrations are one handler.
        let beta = list
            .iter()
            .find(|cs| cs["text"] == "beta")
            .expect("beta is in the list")
            .clone();
        let (code, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &admin,
            &format!("{}/delete", recent_path(&user.id)),
            Some(&beta),
        )
        .await;
        assert_eq!(code, 200, "{base} on the POST form: {raw}");
        let after = recents(&http, base, &admin, &user.id).await;
        assert!(!after.contains("beta"), "{base}: {after}");
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

/// The refusals, which are the bulk of what a client can reach.
#[tokio::test]
async fn the_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    // **One `if` holds three conditions**, so a malformed body, an empty status and an expired
    // one are indistinguishable: all three are `custom_status`.
    let bad_bodies = [
        serde_json::json!({}),
        serde_json::json!({"emoji": "", "text": ""}),
        // A duration that is not in the table, with a future expiry.
        serde_json::json!({"emoji": "wave", "text": "x", "duration": "next_tuesday",
                           "expires_at": "2099-01-01T00:00:00Z"}),
        // A valid duration whose expiry is in the past.
        serde_json::json!({"emoji": "wave", "text": "x", "duration": "one_hour",
                           "expires_at": "2000-01-01T00:00:00Z"}),
    ];
    for body in &bad_bodies {
        for base in [GO, RUST] {
            let (code, raw) = send(
                &http,
                reqwest::Method::PUT,
                base,
                &admin,
                &custom_path(me),
                Some(body),
            )
            .await;
            assert_eq!(code, 400, "{base} on {body}: {raw}");
            let parsed: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
            assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
        }
    }

    // An emoji nothing defines. The wrapper is a **400**, replacing the emoji route's own 404.
    for base in [GO, RUST] {
        let (code, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &admin,
            &custom_path(me),
            Some(&serde_json::json!({"emoji": "mmrs-no-such-emoji", "text": "x"})),
        )
        .await;
        assert_eq!(code, 400, "{base} on an unknown emoji: {raw}");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            parsed["id"], "api.custom_status.set_custom_statuses.emoji_not_found",
            "{base}"
        );
    }

    // A malformed id in the path.
    for base in [GO, RUST] {
        let (code, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &custom_path("short"),
            None,
        )
        .await;
        assert_eq!(code, 400, "{base} on a malformed user_id: {raw}");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    }

    // The recents delete has its **own** decode condition, so an undecodable body names
    // `recent_custom_status` rather than sharing the setter's error.
    for base in [GO, RUST] {
        let response = http
            .delete(format!("{base}{}", recent_path(me)))
            .header("Authorization", format!("Bearer {admin}"))
            .header("Content-Type", "application/json")
            .body("{not json")
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 400, "{base} on a bad body");
        let parsed: serde_json::Value = response.json().await.expect("an AppError");
        assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
    }
}

#[tokio::test]
async fn setting_another_users_custom_status_needs_edit_other_users() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "csperm").await;
    let plain = login_plain_user(&http, "csperm").await;
    let me = common::logged_in_user_id();

    for base in [GO, RUST] {
        for (method, path, body) in [
            (
                reqwest::Method::PUT,
                custom_path(me),
                Some(serde_json::json!({"emoji": "wave", "text": "x"})),
            ),
            (reqwest::Method::DELETE, custom_path(me), None),
            (
                reqwest::Method::DELETE,
                recent_path(me),
                Some(serde_json::json!({"emoji": "wave", "text": "x"})),
            ),
        ] {
            let (code, raw) = send(&http, method.clone(), base, &plain, &path, body.as_ref()).await;
            assert_eq!(code, 403, "{base} {method} {path}: {raw}");
            let parsed: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
            assert_eq!(parsed["id"], "api.context.permissions.app_error");
        }
    }

    // Their own is allowed, and `me` resolves.
    let (code, raw) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &plain,
        &custom_path("me"),
        Some(&serde_json::json!({"emoji": "wave", "text": "mine"})),
    )
    .await;
    assert_eq!(code, 200, "a user may set their own: {raw}");

    delete_plain_user(&http, &admin, &user.id).await;
}

/// The three `user_updated` events, plus the two `UpdatePreferences` publishes.
#[tokio::test]
async fn a_custom_status_publishes_the_same_events_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    // Serialised against every other broadcast-counting test: this one asserts a *count* of
    // frames on the shared admin's stream, which is only true while nothing else writes to
    // that user. See `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let mut go_socket = SocketProbe::connect(GO, &admin).await;
    let mut rust_socket = SocketProbe::connect(RUST, &admin).await;

    send(
        &http,
        reqwest::Method::PUT,
        GO,
        &admin,
        &custom_path(me),
        Some(&serde_json::json!({"emoji": "wave", "text": "go event"})),
    )
    .await;
    wait_for_update(&mut go_socket, me, "go event").await;

    send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &admin,
        &custom_path(me),
        Some(&serde_json::json!({"emoji": "wave", "text": "rust event"})),
    )
    .await;
    wait_for_update(&mut rust_socket, me, "rust event").await;

    // **The subject is omitted from two of the three**, so the admin's own socket sees exactly
    // one `user_updated` — the third, addressed to them by id. A port that dropped the third
    // would leave the user who made the change unaware of it.
    //
    // Scoped by the status text, not just by the subject: see [`updates_for`].
    let go_updated = updates_for(go_socket.events_named("user_updated"), me, "go event");
    let rust_updated = updates_for(rust_socket.events_named("user_updated"), me, "rust event");
    assert_eq!(
        go_updated.len(),
        rust_updated.len(),
        "different numbers of user_updated reached the subject: go {:?} rust {:?}",
        go_socket.raw,
        rust_socket.raw
    );
    assert_eq!(
        go_updated.len(),
        1,
        "the omit map leaves exactly the subject's own copy: {:?}",
        go_socket.raw
    );
    assert_eq!(
        go_updated[0]["broadcast"], rust_updated[0]["broadcast"],
        "the surviving event is addressed differently"
    );

    // `Sanitize(nil)` keeps the profile fields `SanitizeProfile` strips, so the subject's copy
    // still carries their email.
    let go_user = &go_updated[0]["data"]["user"];
    let rust_user = &rust_updated[0]["data"]["user"];
    let go_keys: Vec<&String> = go_user.as_object().expect("a user").keys().collect();
    let rust_keys: Vec<&String> = rust_user.as_object().expect("a user").keys().collect();
    assert_eq!(go_keys, rust_keys, "the embedded user's keys differ");
    assert_eq!(rust_user["id"], me);
    // `password` carries `omitempty` and `Sanitize` blanks it, so the key is **absent** on both
    // rather than present-and-empty — which the key-set comparison above already pinned, and
    // which this states outright because it is the thing that would matter if it changed.
    assert!(
        go_user.get("password").is_none() && rust_user.get("password").is_none(),
        "no password key on the wire: go {go_user} rust {rust_user}"
    );

    // And `addRecentCustomStatus` goes through `UpdatePreferences`, which publishes its own pair.
    for (label, socket) in [("go", &go_socket), ("rust", &rust_socket)] {
        assert_eq!(
            socket.events_named("preferences_changed").len(),
            1,
            "{label} published one preferences_changed: {:?}",
            socket.raw
        );
        assert_eq!(
            socket.events_named("sidebar_category_updated").len(),
            1,
            "{label} published one sidebar_category_updated: {:?}",
            socket.raw
        );
    }

    // `preferences` is a **string** holding JSON, not an array.
    let go_prefs = go_socket.events_named("preferences_changed");
    let rust_prefs = rust_socket.events_named("preferences_changed");
    assert!(
        go_prefs[0]["data"]["preferences"].is_string(),
        "Go sends a string: {:?}",
        go_prefs[0]
    );
    assert!(
        rust_prefs[0]["data"]["preferences"].is_string(),
        "and so must we: {:?}",
        rust_prefs[0]
    );

    // Leave the admin without a custom status, for the suites that read their profile.
    send(
        &http,
        reqwest::Method::DELETE,
        RUST,
        &admin,
        &custom_path(me),
        None,
    )
    .await;
    send(
        &http,
        reqwest::Method::DELETE,
        GO,
        &admin,
        &custom_path(me),
        None,
    )
    .await;
}

/// The two *broadcast* copies, which the subject's own socket can never see.
///
/// `sendUpdatedUserEvent`'s first two events omit the subject, so a suite watching only the
/// admin's socket sees exactly one `user_updated` and cannot tell `ContainsSensitiveData` from
/// `ContainsSanitizedData` at all — measured: a mutation swapping them survived. A **second**
/// socket, belonging to a user without `manage_system`, is the fixture that can: it must receive
/// the member copy and *not* the admin copy, so exactly one arrives.
#[tokio::test]
async fn a_non_admin_socket_receives_exactly_one_of_the_two_broadcast_copies() {
    if !stack_enabled() {
        return;
    }
    // Serialised against every other broadcast-counting test: this one asserts a *count* of
    // frames on the shared admin's stream, which is only true while nothing else writes to
    // that user. See `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let watcher = create_plain_user(&http, &admin, &team, "cswatch").await;
    let plain = login_plain_user(&http, "cswatch").await;
    let me = common::logged_in_user_id();

    let mut go_socket = SocketProbe::connect(GO, &plain).await;
    let mut rust_socket = SocketProbe::connect(RUST, &plain).await;

    // Change the **admin's** status, so the watcher is not the omitted subject.
    send(
        &http,
        reqwest::Method::PUT,
        GO,
        &admin,
        &custom_path(me),
        Some(&serde_json::json!({"emoji": "wave", "text": "go broadcast"})),
    )
    .await;
    wait_for_update(&mut go_socket, me, "go broadcast").await;

    send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &admin,
        &custom_path(me),
        Some(&serde_json::json!({"emoji": "wave", "text": "rust broadcast"})),
    )
    .await;
    wait_for_update(&mut rust_socket, me, "rust broadcast").await;

    // **Filtered by subject *and* by status text, not counted outright.** This socket is open for
    // the whole test and other suites create and delete users on the same server, each of which
    // publishes its own `user_updated` — measured: an unfiltered count saw two, and the second
    // belonged to another test's fixture. Filtering by subject alone was still not enough once
    // the server got fast enough for this file's own siblings to overlap; see [`updates_for`].
    let go_updated = updates_for(go_socket.events_named("user_updated"), me, "go broadcast");
    let rust_updated = updates_for(
        rust_socket.events_named("user_updated"),
        me,
        "rust broadcast",
    );
    assert_eq!(
        go_updated.len(),
        1,
        "Go sends a non-admin exactly the sanitized copy: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_updated.len(),
        1,
        "and so must we — two means the sensitive copy leaked to a non-admin: {:?}",
        rust_socket.raw
    );
    // And it is the *sanitized* one, which is the flag the mutation swaps.
    assert_eq!(go_updated[0]["broadcast"]["contains_sanitized_data"], true);
    assert_eq!(
        rust_updated[0]["broadcast"]["contains_sanitized_data"],
        true
    );

    // It is the member copy, so `SanitizeProfile(_, false)` has run: `auth_data` is blanked and
    // `notify_props` is emptied, which the admin copy keeps.
    let go_user = &go_updated[0]["data"]["user"];
    let rust_user = &rust_updated[0]["data"]["user"];
    let go_keys: Vec<&String> = go_user.as_object().expect("a user").keys().collect();
    let rust_keys: Vec<&String> = rust_user.as_object().expect("a user").keys().collect();
    assert_eq!(go_keys, rust_keys, "the member copy's keys differ");
    assert_eq!(rust_user["id"], me);

    send(
        &http,
        reqwest::Method::DELETE,
        RUST,
        &admin,
        &custom_path(me),
        None,
    )
    .await;
    send(
        &http,
        reqwest::Method::DELETE,
        GO,
        &admin,
        &custom_path(me),
        None,
    )
    .await;
    delete_plain_user(&http, &admin, &watcher.id).await;
}

/// With the feature off the four routes answer **501 with a body**, not a silent success.
#[tokio::test]
async fn the_configuration_gate_answers_501_on_all_four() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(server) = common::SecondServer::start(
        8074,
        &[("MM_TEAMSETTINGS_ENABLECUSTOMUSERSTATUSES", "false")],
    )
    .await
    else {
        return;
    };

    for (method, path, body) in [
        (
            reqwest::Method::PUT,
            custom_path(me),
            Some(serde_json::json!({"emoji": "wave", "text": "x"})),
        ),
        (reqwest::Method::DELETE, custom_path(me), None),
        (
            reqwest::Method::DELETE,
            recent_path(me),
            Some(serde_json::json!({"emoji": "wave", "text": "x"})),
        ),
        (
            reqwest::Method::POST,
            format!("{}/delete", recent_path(me)),
            Some(serde_json::json!({"emoji": "wave", "text": "x"})),
        ),
    ] {
        let (code, raw) = send(
            &http,
            method.clone(),
            &server.base,
            &admin,
            &path,
            body.as_ref(),
        )
        .await;
        assert_eq!(code, 501, "{method} {path} with the feature off: {raw}");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(parsed["id"], "api.custom_status.disabled");
    }

    // And the gate runs **before** the decode: a malformed body is still a 501, not a 400.
    let response = http
        .put(format!("{}{}", server.base, custom_path(me)))
        .header("Authorization", format!("Bearer {admin}"))
        .header("Content-Type", "application/json")
        .body("{not json")
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response.status().as_u16(),
        501,
        "the gate precedes the decode"
    );
}
