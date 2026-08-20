//! Cross-server parity for `GET /api/v4/teams/{team_id}/stats`.
//!
//! Three claims carry the route, and each is the kind a reader would get wrong from the
//! neighbouring handlers: the gate is `view_team` with **no** public-team fallback (unlike
//! `getTeam`); the team is **never fetched**, so a missing id is a 200 of zeroes for an admin
//! (unlike `getChannelStats`, whose gate's own lookup 403s the same shape); and "total" counts
//! deactivated users while "active" does not.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_team_stats
//! ```
//!
//! The restrictions fast path (`view_members` held system-wide → nil restrictions) is true for
//! every caller in this deployment, so the **forward** taken when it fails is unreachable here:
//! exercising it needs `view_members` stripped from the `system_user` role, a global mutation no
//! shared-database test should make. Recorded as transcribed-not-measured in MIGRATION.md.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// Create a team through Go's API and return its id (same helper as `parity_team_get.rs`).
async fn create_team(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Soft-delete a user through Go's API; their membership rows survive, which is the point.
async fn deactivate_user(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deactivating {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// The split the route exists to report: a deactivated member's surviving row counts in
/// `total_member_count` and not in `active_member_count`. Byte-identical, newline included, and
/// the two numbers differ — equal counters could not catch a swapped wiring.
#[tokio::test]
async fn the_stats_body_is_byte_identical_and_the_two_counts_differ() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "teamstats").await;
    let member = create_plain_user(&client, &token, &team_id, "teamstats").await;
    deactivate_user(&client, &token, &member.id).await;

    let path = format!("/api/v4/teams/{team_id}/stats");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_plain_user(&client, &token, &member.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "the encoder's newline ([D-086])"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["team_id"].as_str(), Some(team_id.as_str()));
    // Not exact numbers: concurrently running suites resolve "a home team for my plain user" as
    // `teams[0]` of the admin's list — which has no ORDER BY — and can land their users in this
    // fresh team (measured: the first run of this suite gained two foreign members mid-test).
    // Pollution only *adds* members, so the split still shows: the byte comparison above is the
    // oracle, and these guards only keep it non-vacuous.
    let total = parsed["total_member_count"].as_i64().expect("a number");
    let active = parsed["active_member_count"].as_i64().expect("a number");
    assert!(
        total > active,
        "the deactivated member must count in total ({total}) and not in active ({active})"
    );
    assert!(active >= 1, "the admin creator is active");
}

/// A plain member holds `view_team` through `team_user` and `view_members` through
/// `system_user`, so the route serves locally for the least-privileged real caller.
#[tokio::test]
async fn a_plain_member_gets_the_stats_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "teamstatsmem").await;
    let member = create_plain_user(&client, &token, &team_id, "teamstatsmem").await;

    let path = format!("/api/v4/teams/{team_id}/stats");
    let (go_body, rs_body) = fetch_both_stable(&client, &member.token, &path).await;
    delete_plain_user(&client, &token, &member.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert!(
        parsed["total_member_count"].as_i64().expect("a number") >= 2,
        "at least the creator and this member — foreign joins can only add"
    );
}

/// No public-team fallback here: the same non-member who can read a **public** team's body
/// through `getTeam`'s `list_public_teams` fallback is refused its stats — the gate is
/// `view_team` alone. Both refusals, compared field by field.
#[tokio::test]
async fn a_non_member_is_403_even_on_a_public_team() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    // The outsider gets a home team of its own rather than `teams[0]` of the admin's unordered
    // list — which mid-run can resolve to another test's fresh fixture team and pollute its
    // member counts (measured; see the first test's comment).
    let home_team = create_team(&client, &token, "teamstatsouthome").await;
    let team_id = create_team(&client, &token, "teamstatspub").await;
    // Make it public, so a 403 here can only mean "stats has no public fallback".
    let response = client
        .put(format!("{GO}/api/v4/teams/{team_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "allow_open_invite": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "patching the team public");
    let outsider = create_plain_user(&client, &token, &home_team, "teamstatsout").await;

    let path = format!("/api/v4/teams/{team_id}/stats");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    delete_plain_user(&client, &token, &outsider.id).await;

    assert_eq!(
        go_status, 403,
        "getTeam would serve this caller; stats must not"
    );
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "stats non-member");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
}

/// Nothing here fetches the team, and `SessionHasPermissionToTeam` reads only the session's
/// memberships and roles — so a missing id splits by caller: a **200 of zeroes** for the admin,
/// a 403 for a plain user. The opposite of `getChannelStats` on the same shape, and both halves
/// are Go's, asserted in one test so neither reads as an accident.
#[tokio::test]
async fn a_missing_team_is_zeroes_for_the_admin_and_403_for_a_plain_user() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    // Own home team, same reason as the public-team test: never join the admin's `teams[0]`.
    let home_team = create_team(&client, &token, "teamstatsmisshome").await;
    let plain = create_plain_user(&client, &token, &home_team, "teamstatsmiss").await;

    let path = "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/stats";

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 200, "no fetch anywhere, so nothing 404s");
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the zeroes must match byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["total_member_count"].as_i64(), Some(0));
    assert_eq!(
        parsed["team_id"].as_str(),
        Some("zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        "the id is the caller's segment echoed back"
    );

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &plain.token, path).await;
    delete_plain_user(&client, &token, &plain.id).await;
    assert_eq!(
        go_status, 403,
        "no membership, no system grant — the gate denies"
    );
    assert_eq!(rs_status, 403);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing team, plain user");
}

/// [D-150]: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the mux 404 must come
/// from Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/teams/no-pe/stats";

    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "Go would not have routed this, so we must not handle it"
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();
    assert_eq!(status, 404);

    let direct = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status);
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );
}

/// Only GET is ours; Go registers nothing else on this path, so a POST is gorilla's own answer,
/// reached through the forward.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "teamstatspost").await;
    let path = format!("/api/v4/teams/{team_id}/stats");

    let response = client
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST is not migrated, so it must be forwarded"
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();

    let direct = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status, "Go's own method answer");
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );
}
