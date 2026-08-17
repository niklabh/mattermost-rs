//! Cross-server parity for `GET /api/v4/users/me/teams/members`.
//!
//! Notable because it is the first route migrated *past* a permission check rather than around
//! one: Go guards `SanitizeRoleData` with `SessionHasPermissionToTeam`, and the permission system
//! is unported — but the sanitiser is a no-op for one's own membership, so the guard cannot
//! change this route's output. These tests hold that reasoning to the running Go server.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_team_members_route
//! ```

mod common;

use common::{RUST, client, fetch_both, go_minted_token, stack_enabled};

const PATH: &str = "/api/v4/users/me/teams/members";

#[tokio::test]
async fn team_members_are_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go_body, rs_body) = fetch_both(&client, &token, PATH).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    // Not vacuous: two empty arrays would also be identical.
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert!(
        !parsed.as_array().expect("an array").is_empty(),
        "the fixture user belongs to no team, so this comparison proves nothing"
    );
}

/// The reasoning that makes this route portable, checked against Go rather than asserted: our own
/// memberships come back with their role data **intact**, because `SanitizeRoleData` does nothing
/// for `UserId == currentUserId`. If Go were stripping them, this would catch it.
#[tokio::test]
async fn ones_own_role_data_survives_on_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go_body, rs_body) = fetch_both(&client, &token, PATH).await;

    for (label, body) in [("go", go_body), ("rust", rs_body)] {
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("decodes");
        for member in parsed.as_array().expect("an array") {
            assert!(
                !member["roles"].as_str().unwrap_or_default().is_empty(),
                "{label}: roles were stripped from the caller's own membership"
            );
            assert_ne!(
                member["delete_at"], -1,
                "{label}: -1 is the sanitised sentinel and must not appear for self"
            );
        }
    }
}

/// `json.Marshal`, not an encoder — no trailing newline, unlike `/users/me`.
#[tokio::test]
async fn the_body_has_no_trailing_newline() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go_body, rs_body) = fetch_both(&client, &token, PATH).await;

    assert_ne!(go_body.last(), Some(&b'\n'));
    assert_ne!(rs_body.last(), Some(&b'\n'));
}

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

/// `GET /api/v4/users/me/teams` — the sibling route — is **not** migrated and must still be
/// forwarded. Its `SanitizeTeam` strips `email` and `invite_id` based on two team-scoped
/// permissions we cannot evaluate, so serving it here would leak an invite id. See D-094.
#[tokio::test]
async fn the_sibling_teams_route_is_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let response = client
        .get(format!("{RUST}/api/v4/users/me/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "/users/me/teams needs the permission system and must not be served here"
    );
}
