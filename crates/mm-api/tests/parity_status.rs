//! Cross-server parity for `GET /api/v4/users/{user_id}/status` and
//! `POST /api/v4/users/status/ids`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_status
//! ```
//!
//! # Why every fixture user here is freshly created
//!
//! Go answers from its status cache first and the table second; the port reads the table. They
//! agree for any status written through `PUT /users/{id}/status` (cache and row together) and for
//! any user with **no** status at all (miss in both, synthesised `offline`). They can disagree for
//! a user Go has *seen* — `SetActiveChannel` and the websocket presence paths update the cache
//! without touching the row. The fixture admin is exactly such a user (every other parity suite
//! views channels as it), so it is never the subject here, only the actor.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both, fetch_both_raw, go_minted_token, post_both_raw,
    purge_api_fixtures, set_user_status, stack_enabled,
};

/// Three users in three states: a timed DND (every field non-zero, including the seconds-based
/// `dnd_end_time`), a manual `away`, and one with no row at all.
struct Fixture {
    admin_token: String,
    dnd: common::PlainUser,
    away: common::PlainUser,
    none: common::PlainUser,
    /// Go's own `getUserStatus` body for the DND user, captured from the PUT.
    go_put_body_for_dnd: Vec<u8>,
}

async fn fixture(client: &reqwest::Client, tag: &str) -> Fixture {
    purge_api_fixtures().await;
    let admin_token = go_minted_token(client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, &admin_token).await;

    let dnd = create_plain_user(client, &admin_token, &team_id, &format!("{tag}dnd")).await;
    let away = create_plain_user(client, &admin_token, &team_id, &format!("{tag}away")).await;
    let none = create_plain_user(client, &admin_token, &team_id, &format!("{tag}none")).await;

    // A DND end time an hour out, in **seconds** — Go truncates it to the minute, which the
    // byte comparison below then has to agree with.
    let end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64
        + 3_600;
    let go_put_body_for_dnd = set_user_status(client, &dnd.token, &dnd.id, "dnd", end).await;
    set_user_status(client, &away.token, &away.id, "away", 0).await;

    Fixture {
        admin_token,
        dnd,
        away,
        none,
        go_put_body_for_dnd,
    }
}

async fn teardown(client: &reqwest::Client, f: &Fixture) {
    for user in [&f.dnd, &f.away, &f.none] {
        delete_plain_user(client, &f.admin_token, &user.id).await;
    }
}

/// The single-user route, for a user whose row is fully populated. Byte-identical, **with** the
/// encoder newline — and identical to what Go's PUT answered, which is the same handler.
#[tokio::test]
async fn a_populated_status_is_byte_identical_including_the_newline() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "stpop").await;

    let path = format!("/api/v4/users/{}/status", f.dnd.id);
    let (go_body, rs_body) = fetch_both(&client, &f.admin_token, &path).await;
    teardown(&client, &f).await;

    assert_eq!(rs_body, go_body, "the two servers must agree byte for byte");
    assert_eq!(rs_body, f.go_put_body_for_dnd, "and with Go's PUT response");
    assert_eq!(rs_body.last(), Some(&b'\n'), "Encode appends a newline");

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["status"], "dnd");
    assert_eq!(parsed["manual"], true);
    assert_ne!(
        parsed["dnd_end_time"], 0,
        "the fixture must exercise the field"
    );
    assert!(
        parsed.get("active_channel").is_none(),
        "never present from a table read"
    );
    assert!(parsed.get("prev_status").is_none(), "json:\"-\"");
}

/// A user with no `Status` row is **200 offline**, not 404 — the route goes through the list
/// lookup, which synthesises the miss. So is an id that names no user at all, and so is a
/// 26-character string that is not even a valid id: only the length is checked.
#[tokio::test]
async fn a_user_with_no_row_and_an_unknown_id_are_both_offline() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "stnone").await;

    let path = format!("/api/v4/users/{}/status", f.none.id);
    let (go_body, rs_body) = fetch_both(&client, &f.admin_token, &path).await;
    assert_eq!(rs_body, go_body);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["status"], "offline");
    assert_eq!(parsed["last_activity_at"], 0);

    let (go_unknown, rs_unknown) = fetch_both(
        &client,
        &f.admin_token,
        "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/status",
    )
    .await;
    assert_eq!(
        rs_unknown, go_unknown,
        "an id that is no user is still offline"
    );
    teardown(&client, &f).await;
}

/// `me` resolves to the caller — and the plain user has a row, so this is not a vacuous match
/// of two offline bodies.
#[tokio::test]
async fn me_resolves_to_the_caller() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "stme").await;

    let (go_body, rs_body) = fetch_both(&client, &f.away.token, "/api/v4/users/me/status").await;
    teardown(&client, &f).await;

    assert_eq!(rs_body, go_body);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["user_id"], f.away.id.as_str());
    assert_eq!(parsed["status"], "away");
}

/// An id that is the wrong length is `RequireUserId`'s 400; one outside the mux charset is
/// forwarded so Go's own 404 answers.
#[tokio::test]
async fn a_malformed_id_segment_matches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/users/tooshort/status").await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "short id");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    let response = client
        .get(format!("{RUST}/api/v4/users/not-an-id/status"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a hyphen is outside Go's mux charset; forwarded (D-150)"
    );
    assert_eq!(response.status(), 404);
}

/// The list route: three known users in three states plus an unknown id, asked for out of
/// order and with a duplicate. Byte-identical **without** a trailing newline (`json.Marshal`),
/// which also pins the order: found ids sorted, then the synthesised ones.
#[tokio::test]
async fn the_list_is_byte_identical_without_a_newline() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "stlist").await;

    let body = serde_json::to_vec(&serde_json::json!([
        f.none.id,
        f.dnd.id,
        "zzzzzzzzzzzzzzzzzzzzzzzzzz",
        f.away.id,
        f.dnd.id,
    ]))
    .expect("serialises");

    // Go reads its cache first: warm it — the same request, once — so the order compared is
    // the steady state every real client sees rather than the one-off cold order.
    let _ = client
        .post(format!("{GO}/api/v4/users/status/ids"))
        .header("Authorization", format!("Bearer {}", f.admin_token))
        .header("Content-Type", "application/json")
        .body(body.clone())
        .send()
        .await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.admin_token, "/api/v4/users/status/ids", &body).await;
    teardown(&client, &f).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte, order included"
    );
    assert_ne!(rs_body.last(), Some(&b'\n'), "Marshal appends nothing");

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let list = parsed.as_array().expect("an array");
    assert_eq!(
        list.len(),
        4,
        "the duplicate is collapsed; the unknown id is answered"
    );
    let states: Vec<&str> = list
        .iter()
        .map(|s| s["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(states.iter().filter(|s| **s == "offline").count(), 2);
    assert!(states.contains(&"dnd"));
    assert!(states.contains(&"away"));
}

/// The three validation branches, each compared against Go's refusal.
#[tokio::test]
async fn the_list_routes_refusals_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (body, expected_id, context) in [
        (
            &b"not json"[..],
            "api.payload.parse.error",
            "unparseable body",
        ),
        (&b"[1, 2]"[..], "api.payload.parse.error", "numbers"),
        (
            &b"[]"[..],
            "api.context.invalid_body_param.app_error",
            "empty array",
        ),
        (
            &b"null"[..],
            "api.context.invalid_body_param.app_error",
            "null",
        ),
        (
            &b"[\"tooshort\"]"[..],
            "api.context.invalid_body_param.app_error",
            "wrong length",
        ),
        (
            &b"[\"zzzzzzzzzzzzzzzzzzzzzzzzzz\", null]"[..],
            "api.context.invalid_body_param.app_error",
            "a null element",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, "/api/v4/users/status/ids", body).await;
        assert_eq!(go_status, 400, "{context}: Go");
        assert_eq!(rs_status, 400, "{context}: Rust");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        assert_eq!(go["id"], expected_id, "{context}");
    }

    // Trailing bytes after the array are never read — a 200, not a 400.
    let ((go_status, _), (rs_status, _)) = post_both_raw(
        &client,
        &token,
        "/api/v4/users/status/ids",
        b"[\"zzzzzzzzzzzzzzzzzzzzzzzzzz\"] trailing",
    )
    .await;
    assert_eq!(go_status, 200, "Go's Decode stops after the first value");
    assert_eq!(rs_status, 200, "and so must ours");
}

/// A `GET` on the POST-only literal keeps being forwarded, so Go's own answer stands as before.
#[tokio::test]
async fn get_on_the_ids_literal_is_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let response = client
        .get(format!("{RUST}/api/v4/users/status/ids"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go")
    );
    let rs_status = response.status().as_u16();
    let go_status = client
        .get(format!("{GO}/api/v4/users/status/ids"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable")
        .status()
        .as_u16();
    assert_eq!(rs_status, go_status);
    // Measured: Go answers **404**, not the 405 gorilla's method-mismatch would suggest — api4
    // installs its own not-found handler and the mismatch falls through to it. Pinned so a
    // future "fix" that answers 405 locally is caught.
    assert_eq!(go_status, 404);
}
