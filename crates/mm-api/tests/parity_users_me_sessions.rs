//! Cross-server parity for `GET /api/v4/users/me/sessions`.
//!
//! The slice's first **list** response, and its first endpoint whose body is a credential hazard:
//! every element carries the token that authenticates it, and Go strips them with
//! `session.Sanitize()` before writing. Both properties are asserted here.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_users_me_sessions
//! ```

mod common;

use common::{RUST, client, fetch_both, fetch_both_stable, go_minted_token, stack_enabled};

const PATH: &str = "/api/v4/users/me/sessions";

/// Byte-for-byte, including element order — `ORDER BY LastActivityAt DESC` is part of the
/// response, not an implementation detail.
///
/// Through [`fetch_both_stable`], not `fetch_both`: each session embeds `team_members`, whose
/// query has **no `ORDER BY`**, so its element order is heap order — stable while the table is
/// quiet, but a concurrently running suite that inserts or soft-deletes the admin's membership
/// rows can reorder it between Go's read and ours. Measured, not theoretical: the `getTeam`
/// session's team-churning fixtures flipped two memberships in exactly that window and this test
/// reported a divergence while both servers were correct for the instant each read.
#[tokio::test]
async fn sessions_list_is_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go_body, rs_body) = fetch_both_stable(&client, &token, PATH).await;

    // Byte-identical is the first-chance assertion, but `team_members` is exempt from *order*:
    // its query has no `ORDER BY`, Go serves the order its **session cache** hydrated at some
    // earlier table state, and we re-read fresh — so after membership churn the two orders can
    // differ *stably*, which the Go–Rust–Go window cannot detect. Two Go servers could disagree
    // with each other the same way, so the order is not a parity property; the *content* is.
    // Measured twice: a live run during the `getTeam` session, then a no-op mutation control
    // failing on this test during the `getTeamStats` session.
    if rs_body != go_body {
        let go: serde_json::Value = serde_json::from_slice(&go_body).expect("go decodes");
        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("rust decodes");
        assert_eq!(
            with_sorted_team_members(go),
            with_sorted_team_members(rs),
            "the two servers differ beyond team_members order"
        );
    }

    // Guard against a vacuous pass: two empty arrays are also byte-identical.
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("body decodes");
    let list = parsed.as_array().expect("the response is an array");
    assert!(
        !list.is_empty(),
        "the fixture user has no sessions, so the comparison proves nothing"
    );
}

/// Each session's `team_members`, sorted by `team_id` — the only normalisation applied before
/// the structural fallback comparison above. Everything else stays exactly as served.
fn with_sorted_team_members(mut sessions: serde_json::Value) -> serde_json::Value {
    if let Some(list) = sessions.as_array_mut() {
        for session in list {
            if let Some(members) = session
                .get_mut("team_members")
                .and_then(serde_json::Value::as_array_mut)
            {
                members.sort_by_key(|m| m["team_id"].as_str().unwrap_or_default().to_owned());
            }
        }
    }
    sessions
}

/// The security property. Asserted against the **live** response rather than a constructed one,
/// because the thing that can go wrong is the handler forgetting to call `sanitize`, not
/// `sanitize` itself.
#[tokio::test]
async fn no_session_token_is_ever_returned() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, rs_body) = fetch_both(&client, &token, PATH).await;
    let body = String::from_utf8_lossy(&rs_body);

    assert!(
        !body.contains(&token),
        "our own bearer token came back in the response body"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("body decodes");
    for session in parsed.as_array().expect("an array") {
        assert_eq!(
            session["token"], "",
            "every session's token must be blanked, not merely the caller's"
        );
    }
}

/// Go writes this handler with `json.Marshal` + `w.Write`, not `json.NewEncoder().Encode()`, so
/// there is **no** trailing newline — the opposite of `/users/me`. Same wire type, same server,
/// different call site. See D-086.
#[tokio::test]
async fn the_sessions_body_has_no_trailing_newline_unlike_users_me() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let (go_sessions, rs_sessions) = fetch_both(&client, &token, PATH).await;
    assert_ne!(
        go_sessions.last(),
        Some(&b'\n'),
        "Go's Marshal adds nothing"
    );
    assert_ne!(rs_sessions.last(), Some(&b'\n'), "so neither may we");

    // And the contrast that makes the point: the other route does end in one.
    let (go_me, rs_me) = fetch_both(&client, &token, "/api/v4/users/me").await;
    assert_eq!(go_me.last(), Some(&b'\n'), "Go's Encode appends one");
    assert_eq!(rs_me.last(), Some(&b'\n'), "and so must we");
}

/// `team_members` must be `[]` rather than `null` — Go allocates the slice with `make`, so the
/// key is always a well-formed array even for a user in no teams.
#[tokio::test]
async fn team_members_is_an_array_not_null() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, rs_body) = fetch_both(&client, &token, PATH).await;

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("body decodes");
    for session in parsed.as_array().expect("an array") {
        assert!(
            session["team_members"].is_array(),
            "team_members must be an array, got {}",
            session["team_members"]
        );
    }
}

/// The route is served here, not forwarded.
#[tokio::test]
async fn the_route_is_served_by_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let response = client
        .get(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust")
    );
}
