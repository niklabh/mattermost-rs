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

use common::{RUST, client, fetch_both, fetch_both_stable, go_minted_token, stack_enabled};

const PATH: &str = "/api/v4/users/me/teams/members";

/// Through [`fetch_both_stable`]: the membership query has no `ORDER BY`, so a concurrently
/// running suite that churns the admin's `TeamMembers` rows can reorder the list between the two
/// reads — the same measured flake as the sessions suite, whose body embeds this list.
#[tokio::test]
async fn team_members_are_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go_body, rs_body) = fetch_both_stable(&client, &token, PATH).await;

    // Byte-identical first; on a mismatch, compare with the list order normalised. This whole
    // response *is* the unordered `GetTeamsForUser` result, so its element order is heap order —
    // not a parity property, for the same measured reason as the sessions suite (whose bodies
    // embed this list): Go can serve a cached hydration's order while we re-read fresh.
    if rs_body != go_body {
        let sort = |body: &[u8]| {
            let mut value: serde_json::Value = serde_json::from_slice(body).expect("decodes");
            value
                .as_array_mut()
                .expect("an array")
                .sort_by_key(|m| m["team_id"].as_str().unwrap_or_default().to_owned());
            value
        };
        assert_eq!(
            sort(&go_body),
            sort(&rs_body),
            "the two servers differ beyond element order"
        );
    }

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

/// `GET /api/v4/users/me/teams` — the sibling route — spent two phases forwarded because its
/// `SanitizeTeam` needs `SessionHasPermissionToTeam` ([D-094]). That checker exists now and the
/// route is served from Rust; this test used to hold it forwarded and now holds the opposite, so
/// a registration typo cannot silently fall back to the proxy and pass every byte comparison
/// with Go answering both sides. The route's own suite is `parity_teams_for_user.rs`.
#[tokio::test]
async fn the_sibling_teams_route_is_now_served_here() {
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
        Some("rust"),
        "SessionHasPermissionToTeam landed; this route is ours now"
    );
}
