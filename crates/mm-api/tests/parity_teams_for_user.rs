//! Cross-server parity for `GET /api/v4/users/{user_id}/teams`.
//!
//! The route [D-094] classified as **not escapable** and kept forwarded for two phases:
//! `SanitizeTeam` strips `email` and `invite_id` per two team-scoped permissions with no
//! self-shortcut, so serving it without `SessionHasPermissionToTeam` would have leaked an invite
//! id — which is enough to join the team. That checker exists now, so the interesting parity
//! here is the *sanitisation*: what a plain member's own team list does and does not contain.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_teams_for_user
//! ```
//!
//! Every fixture is created and unwound here; [`common::purge_api_fixtures`] clears what a
//! panicking run leaves behind (teams included — Go's `DELETE /teams/{id}` archives, and an
//! archived team keeps its name).

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id,
    purge_api_fixtures, stack_enabled,
};

/// Create a team through Go's API and return its id. The creator joins automatically.
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

/// Archive a team — Go's `DELETE /teams/{id}` soft-deletes, like the channel one.
async fn archive_team(client: &reqwest::Client, token: &str, team_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving the team failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Remove a user from a team — soft-deletes the membership row.
async fn remove_from_team(client: &reqwest::Client, token: &str, team_id: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}/members/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "removing the member failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn team_ids(body: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
    parsed
        .as_array()
        .expect("an array")
        .iter()
        .map(|t| t["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The `me` alias and the explicit id answer with the same bytes, and the body is
/// `json.Marshal` + `w.Write`: no trailing newline ([D-086]).
#[tokio::test]
async fn me_and_the_explicit_id_are_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let (go_me, rs_me) = fetch_both_stable(&client, &token, "/api/v4/users/me/teams").await;
    let explicit = format!("/api/v4/users/{}/teams", logged_in_user_id());
    let (_, rs_explicit) = fetch_both_stable(&client, &token, &explicit).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_me),
        String::from_utf8_lossy(&go_me),
        "the two servers must agree byte for byte"
    );
    assert_eq!(
        rs_me, rs_explicit,
        "`me` resolves to the session's id before anything else looks at it"
    );

    // Not vacuous: the fixture user is on at least one team, and the Marshal call site owes no
    // trailing newline.
    let parsed: serde_json::Value = serde_json::from_slice(&rs_me).expect("decodes");
    assert!(
        !parsed.as_array().expect("an array").is_empty(),
        "the fixture user belongs to a team, so an empty answer proves nothing"
    );
    assert_ne!(
        rs_me.last(),
        Some(&b'\n'),
        "json.Marshal + w.Write, no encoder"
    );
}

/// **The sanitiser is why this route waited two phases, and a plain member lands in its mixed
/// cell.** The default `team_user` role grants `invite_user` but not `manage_team` — measured,
/// not assumed: the first version of this test expected both fields stripped and Go returned the
/// invite id. So a plain member's own team list keeps `invite_id` and loses only `email`, which
/// is exactly the pairing a crossed transcription gets wrong — asserted against Go byte for
/// byte, and then explicitly, so this fails loudly rather than byte-agreeing on a joint
/// regression.
#[tokio::test]
async fn a_plain_members_own_team_list_is_sanitized() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let reader = create_plain_user(&client, &token, &team_id, "teamsplain").await;

    let (go_body, rs_body) =
        fetch_both_stable(&client, &reader.token, "/api/v4/users/me/teams").await;
    delete_plain_user(&client, &token, &reader.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let teams = parsed.as_array().expect("an array");
    assert!(!teams.is_empty(), "the plain user was added to a team");
    for team in teams {
        assert_eq!(team["email"].as_str(), Some(""), "manage_team is not held");
        assert_ne!(
            team["invite_id"].as_str(),
            Some(""),
            "team_user grants invite_user, so the invite id survives — an empty one here means \
             the sanitiser is stripping a field its permission protects"
        );
    }
}

/// The system admin holds both permissions on every team, so their list is **not** sanitized —
/// the two cells of the sanitiser the plain user cannot reach.
#[tokio::test]
async fn an_admins_own_team_list_keeps_its_invite_ids() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let (go_body, rs_body) = fetch_both_stable(&client, &token, "/api/v4/users/me/teams").await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    for team in parsed.as_array().expect("an array") {
        assert_ne!(
            team["invite_id"].as_str(),
            Some(""),
            "an admin sees the invite id — if this is empty the byte comparison above was \
             agreeing on an over-sanitised answer"
        );
    }
}

/// Asking about another user needs `sysconsole_read_user_management_users`; a plain user is a
/// 403 from both servers, an admin gets the plain user's (sanitised-for-the-admin) list.
#[tokio::test]
async fn asking_about_another_user_needs_the_sysconsole_permission() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let reader = create_plain_user(&client, &token, &team_id, "teamsgate").await;

    // Plain user asking about the admin: denied identically on both servers.
    let about_admin = format!("/api/v4/users/{}/teams", logged_in_user_id());
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader.token, &about_admin).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "plain about admin");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));

    // Admin asking about the plain user: served, byte-identical.
    let about_reader = format!("/api/v4/users/{}/teams", reader.id);
    let (go_list, rs_list) = fetch_both_stable(&client, &token, &about_reader).await;
    delete_plain_user(&client, &token, &reader.id).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_list),
        String::from_utf8_lossy(&go_list),
        "the admin view of another user's teams must match"
    );
}

/// The store's two `DeleteAt = 0` predicates are separate things, and both are REST-reachable
/// here — so both are **measured** rather than transcribed, unlike [D-151]'s type filter:
/// archiving the *team* removes it from the list while the membership row survives, and removing
/// the *membership* removes it while the team survives. Either predicate dropped resurrects one
/// of the two fixtures.
#[tokio::test]
async fn archived_teams_and_removed_memberships_both_disappear() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let reader = create_plain_user(&client, &token, &home_team, "teamsdel").await;

    let archived = create_team(&client, &token, "teamsarch").await;
    let departed = create_team(&client, &token, "teamsleft").await;
    for team in [&archived, &departed] {
        let response = client
            .post(format!("{GO}/api/v4/teams/{team}/members"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({ "team_id": team, "user_id": reader.id }))
            .send()
            .await
            .expect("Go answers");
        assert!(response.status().is_success(), "adding the member failed");
    }

    // Both present first, so their absence below is the predicates' doing and not the fixture's.
    let (_, before) = fetch_both_stable(&client, &reader.token, "/api/v4/users/me/teams").await;
    let ids_before = team_ids(&before);
    assert!(ids_before.contains(&archived) && ids_before.contains(&departed));

    archive_team(&client, &token, &archived).await;
    remove_from_team(&client, &token, &departed, &reader.id).await;

    let (go_after, rs_after) =
        fetch_both_stable(&client, &reader.token, "/api/v4/users/me/teams").await;
    delete_plain_user(&client, &token, &reader.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_after),
        String::from_utf8_lossy(&go_after),
        "the two servers must agree byte for byte"
    );
    let ids_after = team_ids(&rs_after);
    assert!(
        !ids_after.contains(&archived),
        "Teams.DeleteAt = 0: the archived team is gone though the membership row survives"
    );
    assert!(
        !ids_after.contains(&departed),
        "TeamMembers.DeleteAt = 0: the departed team is gone though the team survives"
    );
    assert!(
        ids_after.contains(&home_team),
        "the untouched membership is still there, so the two absences are not an empty list"
    );
}

/// A well-formed id that matches no user is, for an admin, an **empty list** — the store has no
/// user-exists check and Go's handler never looks the user up.
#[tokio::test]
async fn a_nonexistent_user_is_an_empty_list_for_an_admin() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/teams";
    let (go_body, rs_body) = fetch_both_stable(&client, &token, path).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_body, b"[]",
        "no user lookup happens; an empty list, not a 404"
    );
}

/// [D-150] again: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the mux 404 must
/// come from Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/no-pe/teams";

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
