//! Cross-server parity for `PUT /api/v4/users/{user_id}/status`.
//!
//! **The first route whose model is a cache rather than a table.** Three decisions inside
//! `SetStatusOnline` branch on the *previous* status, and the previous status is not the `Status`
//! row — it is whatever the process last put in `platform.statusCache`. `mm-app` now keeps its
//! own, which is [D-191]'s decision; while both servers run they are independent, so **each
//! server is driven and read through itself** here.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity status_writes
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, client, create_plain_user, delete_plain_user, go_minted_token,
    login_plain_user, stack_enabled,
};

async fn set_status(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    let path = format!("/api/v4/users/{user_id}/status");
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), &path);
    }
    (status, response.text().await.expect("a body"))
}

async fn read_status(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
) -> serde_json::Value {
    let response = http
        .get(format!("{base}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("a readable status");
    response.json().await.unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn the_four_statuses_round_trip_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;

    // A user per server: the two status caches are independent while both run, so driving one
    // user through both would compare each server against the other's stale cache rather than
    // against its own behaviour.
    let go_user = create_plain_user(&http, &admin, &team, "statgo").await;
    let rust_user = create_plain_user(&http, &admin, &team, "statrs").await;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        for status in ["online", "away", "dnd", "offline"] {
            let (code, raw) = set_status(
                &http,
                base,
                &admin,
                &user.id,
                &serde_json::json!({"user_id": user.id, "status": status}),
            )
            .await;
            assert_eq!(code, 200, "{base} setting {status}: {raw}");

            // **The answer is `getUserStatus`'s own body**, not `{"status":"OK"}` — Go tail-calls
            // the read handler — and it is encoder-framed.
            assert!(
                raw.ends_with('\n'),
                "{base}: the answer is the read handler's, which is encoder-framed: {raw:?}"
            );
            let answered: serde_json::Value = serde_json::from_str(&raw).expect("a status");
            assert_eq!(
                answered["status"], status,
                "{base} answered a different status than it was asked for"
            );
            assert_eq!(answered["user_id"], user.id);

            // And the same server reads it back.
            let stored = read_status(&http, base, &admin, &user.id).await;
            assert_eq!(stored["status"], status, "{base} did not persist {status}");
        }
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

#[tokio::test]
async fn online_clears_the_manual_flag_and_the_other_three_set_it() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "statmang").await;
    let rust_user = create_plain_user(&http, &admin, &team, "statmanr").await;

    // The route always passes `manual: true`, so three of the four statuses land manual — but
    // `SetStatusOnline` clears it regardless ("for online there's no manual setting"). The flag
    // is on the wire *and* in the row, and the two must agree.
    //
    // **The leading `online` is load-bearing.** Without it the first request lands on a user with
    // no status row, and `SetStatusAwayIfNeeded`'s fallback builds the placeholder with
    // `Manual: manual` — already true — so `status.Manual = manual` is a redundant assignment and
    // a mutation dropping it survives. Going online first clears the flag, which makes the
    // assignment the only thing that can set it again.
    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        for (status, expect_manual) in [
            ("online", false),
            ("away", true),
            ("dnd", true),
            ("offline", true),
            ("online", false),
        ] {
            let (code, raw) = set_status(
                &http,
                base,
                &admin,
                &user.id,
                &serde_json::json!({"user_id": user.id, "status": status}),
            )
            .await;
            assert_eq!(code, 200, "{base} setting {status}: {raw}");

            let answered: serde_json::Value = serde_json::from_str(&raw).expect("a status");
            assert_eq!(
                answered["manual"], expect_manual,
                "{base} answered the wrong manual flag after {status}"
            );

            let (row_status, row_manual, _, _, _) =
                common::status_row(&user.id).await.expect("a status row");
            assert_eq!(
                row_status, status,
                "{base} did not write {status} to the row"
            );
            assert_eq!(
                row_manual, expect_manual,
                "{base} wrote the wrong manual flag to the row after {status}"
            );
        }
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

#[tokio::test]
async fn dnd_records_the_previous_status_and_truncates_its_end_time() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let go_user = create_plain_user(&http, &admin, &team, "statdndg").await;
    let rust_user = create_plain_user(&http, &admin, &team, "statdndr").await;

    // **`dnd_end_time` is seconds, not milliseconds** — the only timestamp in the package that is
    // — and it is truncated **down** to a whole minute by `truncateDNDEndTime`. A value chosen to
    // be mid-minute proves the truncation rather than passing by luck.
    let end_time = 2_000_000_045i64; // 45 seconds past a minute boundary
    let truncated = 2_000_000_040i64;

    for (base, user) in [(GO, &go_user), (RUST, &rust_user)] {
        // Go online first, so `prev_status` has something to record other than the default.
        set_status(
            &http,
            base,
            &admin,
            &user.id,
            &serde_json::json!({"user_id": user.id, "status": "online"}),
        )
        .await;

        let (code, raw) = set_status(
            &http,
            base,
            &admin,
            &user.id,
            &serde_json::json!({
                "user_id": user.id,
                "status": "dnd",
                "dnd_end_time": end_time,
            }),
        )
        .await;
        assert_eq!(code, 200, "{base} setting dnd: {raw}");

        let stored = read_status(&http, base, &admin, &user.id).await;
        assert_eq!(stored["status"], "dnd", "{base} did not store dnd");
        assert_eq!(
            stored["dnd_end_time"], truncated,
            "{base} did not truncate the end time down to a whole minute"
        );

        // `PrevStatus` carries `json:"-"`, so the row is the only place it is visible. The
        // expiry job restores it, which nothing here runs — but writing it is this setter's job
        // and a port that skipped it would look identical on the wire.
        let (row_status, row_manual, row_prev, row_dnd, _) =
            common::status_row(&user.id).await.expect("a status row");
        assert_eq!(row_status, "dnd");
        assert!(
            row_manual,
            "{base}: SetStatusDoNotDisturbTimed forces manual"
        );
        assert_eq!(
            row_prev, "online",
            "{base} did not record the status dnd replaced"
        );
        assert_eq!(row_dnd, truncated, "{base} stored an untruncated end time");
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rust_user.id).await;
}

#[tokio::test]
async fn the_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    // An unknown status — **including `ooo`**, which the model can hold and this route cannot
    // set: the switch has four arms and everything else is `SetInvalidParam("status")`.
    for status in ["ooo", "sleeping", ""] {
        for base in [GO, RUST] {
            let (code, raw) = set_status(
                &http,
                base,
                &admin,
                me,
                &serde_json::json!({"user_id": me, "status": status}),
            )
            .await;
            assert_eq!(code, 400, "{base} on status {status:?}: {raw}");
            let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
            assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
        }
    }

    // A body naming a different user is a **400 naming `user_id`**, and it runs before the
    // permission check — so this is a 400 rather than a 403 even for a caller who could not have
    // updated that user.
    for base in [GO, RUST] {
        let (code, raw) = set_status(
            &http,
            base,
            &admin,
            me,
            &serde_json::json!({"user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "status": "online"}),
        )
        .await;
        assert_eq!(code, 400, "{base} on a mismatched user_id: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // A malformed id in the path.
    for base in [GO, RUST] {
        let (code, raw) = set_status(
            &http,
            base,
            &admin,
            "short",
            &serde_json::json!({"user_id": "short", "status": "online"}),
        )
        .await;
        assert_eq!(code, 400, "{base} on a malformed user_id: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }
}

#[tokio::test]
async fn setting_another_users_status_needs_edit_other_users() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    common::purge_api_fixtures().await;
    let (team, _channel) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "statperm").await;
    let plain = login_plain_user(&http, "statperm").await;

    // The gate is `SessionHasPermissionToUser`, whose refusal names `edit_other_users`.
    for base in [GO, RUST] {
        let (code, raw) = set_status(
            &http,
            base,
            &plain,
            common::logged_in_user_id(),
            &serde_json::json!({
                "user_id": common::logged_in_user_id(),
                "status": "online",
            }),
        )
        .await;
        assert_eq!(
            code, 403,
            "{base}: a plain user cannot set the admin's: {raw}"
        );
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    // And their own is allowed.
    let (code, raw) = set_status(
        &http,
        RUST,
        &plain,
        &user.id,
        &serde_json::json!({"user_id": user.id, "status": "away"}),
    )
    .await;
    assert_eq!(code, 200, "a user may set their own status: {raw}");

    delete_plain_user(&http, &admin, &user.id).await;
}

#[tokio::test]
async fn a_status_change_is_broadcast_to_the_user() {
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

    // Move to a status each server was not already in, so the "did it change" test fires.
    set_status(
        &http,
        GO,
        &admin,
        me,
        &serde_json::json!({"user_id": me, "status": "dnd"}),
    )
    .await;
    go_socket.collect_for(Duration::from_millis(900)).await;

    set_status(
        &http,
        RUST,
        &admin,
        me,
        &serde_json::json!({"user_id": me, "status": "away"}),
    )
    .await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = go_socket.events_named("status_change");
    let rust_events = rust_socket.events_named("status_change");
    assert!(
        !go_events.is_empty(),
        "Go published one: {:?}",
        go_socket.raw
    );
    assert!(
        !rust_events.is_empty(),
        "we published one: {:?}",
        rust_socket.raw
    );

    let go_last = go_events.last().expect("at least one");
    let rust_last = rust_events.last().expect("at least one");

    // Addressed to the **user** and nothing else, so it reaches only that user's own sessions.
    assert_eq!(
        go_last["broadcast"], rust_last["broadcast"],
        "the status event's addressing differs"
    );
    assert_eq!(rust_last["broadcast"]["user_id"], me);
    assert_eq!(rust_last["broadcast"]["channel_id"], "");
    assert_eq!(rust_last["broadcast"]["team_id"], "");

    // **The data carries two plain fields, not the `Status` object** — `status` and `user_id`.
    let go_keys: Vec<&str> = go_last["data"]
        .as_object()
        .expect("a data map")
        .keys()
        .map(String::as_str)
        .collect();
    let rust_keys: Vec<&str> = rust_last["data"]
        .as_object()
        .expect("a data map")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(go_keys, rust_keys, "the status event's data keys differ");
    assert_eq!(rust_last["data"]["user_id"], me);
    assert_eq!(rust_last["data"]["status"], "away");

    // Put it back, so other suites see the admin online.
    set_status(
        &http,
        RUST,
        &admin,
        me,
        &serde_json::json!({"user_id": me, "status": "online"}),
    )
    .await;
    set_status(
        &http,
        GO,
        &admin,
        me,
        &serde_json::json!({"user_id": me, "status": "online"}),
    )
    .await;
}
