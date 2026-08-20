//! Cross-server parity for `GET /api/v4/teams/{team_id}`.
//!
//! The permission block has a shape no channel route has: `view_team` is computed
//! unconditionally, and a **public** team (open type *and* open invite) falls back to the
//! system-wide `list_public_teams` — which plain users hold, so any authenticated user can read
//! a public team they never joined. Both denials name `view_team`. The suite drives every cell:
//! member, non-member × public/non-public, archived, missing, and the sanitiser's three
//! outcomes (admin keeps both fields, member keeps `invite_id` only, non-member keeps neither).
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_team_get
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// Create a team through Go's API and return its id. Created with type `O` and — Go's creation
/// default — `AllowOpenInvite = false`, so a fresh fixture team is **not** public until
/// [`make_team_public`] says so.
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

/// Flip `AllowOpenInvite` on through Go's patch endpoint — the missing half of "public".
async fn make_team_public(client: &reqwest::Client, token: &str, team_id: &str) {
    let response = client
        .put(format!("{GO}/api/v4/teams/{team_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "allow_open_invite": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching the team public failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Archive a team — Go's `DELETE /teams/{id}` soft-deletes.
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

/// The admin's own team, byte for byte — `manage_team` keeps `email`, `invite_user` keeps
/// `invite_id`, so nothing is sanitised and the body is the whole row plus the encoder newline.
#[tokio::test]
async fn an_admins_team_body_is_byte_identical_and_unsanitised() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "getteamadmin").await;

    let path = format!("/api/v4/teams/{team_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;

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
    assert_eq!(parsed["id"].as_str(), Some(team_id.as_str()));
    assert_ne!(parsed["email"].as_str(), Some(""), "the admin keeps email");
    assert_ne!(
        parsed["invite_id"].as_str(),
        Some(""),
        "the admin keeps invite_id"
    );
}

/// A plain member lands in the sanitiser's mixed cell: `team_user` grants `invite_user` but not
/// `manage_team`, so `invite_id` survives while `email` is stripped — the same measured pairing
/// `getTeamsForUser` pinned, now on the single-team route.
#[tokio::test]
async fn a_plain_members_view_keeps_invite_id_and_loses_email() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "getteammember").await;
    let member = create_plain_user(&client, &token, &team_id, "getteammember").await;

    let path = format!("/api/v4/teams/{team_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &member.token, &path).await;
    delete_plain_user(&client, &token, &member.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["email"].as_str(), Some(""), "email is stripped");
    assert_ne!(
        parsed["invite_id"].as_str(),
        Some(""),
        "invite_id survives for a member — the mixed cell"
    );
}

/// A non-member against a **non-public** team (open type, `AllowOpenInvite = false` — the
/// creation default): `view_team` denies, the fallback must not be consulted, 403 from both.
#[tokio::test]
async fn a_non_member_is_403_on_a_non_public_team() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let team_id = create_team(&client, &token, "getteamclosed").await;
    let outsider = create_plain_user(&client, &token, &home_team, "getteamout").await;

    let path = format!("/api/v4/teams/{team_id}");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    delete_plain_user(&client, &token, &outsider.id).await;

    assert_eq!(go_status, 403, "open type alone is not public");
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "closed non-member");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
}

/// The same non-member against the same team made **public**: `list_public_teams` — which the
/// plain `system_user` role holds — admits, and the sanitiser strips both fields, since this
/// caller holds neither team permission. The route the fallback exists for.
#[tokio::test]
async fn a_non_member_reads_a_public_team_fully_sanitised() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let team_id = create_team(&client, &token, "getteampublic").await;
    make_team_public(&client, &token, &team_id).await;
    let outsider = create_plain_user(&client, &token, &home_team, "getteampub").await;

    let path = format!("/api/v4/teams/{team_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &outsider.token, &path).await;
    delete_plain_user(&client, &token, &outsider.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["allow_open_invite"].as_bool(), Some(true));
    assert_eq!(parsed["email"].as_str(), Some(""), "both fields stripped");
    assert_eq!(
        parsed["invite_id"].as_str(),
        Some(""),
        "both fields stripped"
    );
}

/// A well-formed id that matches nothing is a 404 from the fetch — `app.team.get.find.app_error`,
/// the `find` id, before any permission machinery.
#[tokio::test]
async fn a_missing_team_is_404_from_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;

    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing team");
    assert_eq!(go["id"].as_str(), Some("app.team.get.find.app_error"));
}

/// `SqlTeamStore.Get` has no `DeleteAt` filter, so an archived team still answers here — while
/// disappearing from `GET /users/{id}/teams`, whose query filters it. The team-side half of the
/// asymmetry the channel routes pinned.
#[tokio::test]
async fn an_archived_team_still_answers_200_here() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "getteamarch").await;
    archive_team(&client, &token, &team_id).await;

    let path = format!("/api/v4/teams/{team_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "both servers serve the archived team"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(
        parsed["delete_at"].as_i64(),
        Some(0),
        "it really is archived"
    );
}

/// `?as_content_reviewer=true` is forwarded whole — and because Go reads the flag **after**
/// `GetTeam`, the forwarded answer on a *missing* team is the 404, not the license 501. Both
/// subcases asserted against Go's direct answer; unparseable values serve locally.
#[tokio::test]
async fn the_content_reviewer_query_is_forwarded_and_ordering_is_gos() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "getteamreview").await;

    // An existing team with the flag: forwarded, answered by Go's license/config gate.
    // A missing team with the flag: forwarded, and Go's own fetch-first ordering answers 404.
    for (path, expected_status) in [
        (
            format!("/api/v4/teams/{team_id}?as_content_reviewer=true"),
            501,
        ),
        (
            "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz?as_content_reviewer=true".to_owned(),
            404,
        ),
    ] {
        let ours = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the Rust server answers");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path}: the content-reviewer path is not ours to answer"
        );
        let ours_status = ours.status().as_u16();
        let ours_body: serde_json::Value = ours.json().await.expect("decodes");

        let direct = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), ours_status, "{path}");
        let direct_body: serde_json::Value = direct.json().await.expect("decodes");
        assert_eq!(ours_body["id"], direct_body["id"], "{path}");
        assert_eq!(
            ours_status, expected_status,
            "{path}: the flag is read after the fetch, so a missing team is a 404"
        );
    }

    // Unparseable: ParseBool's error case is false, the ordinary route serves locally.
    let path = format!("/api/v4/teams/{team_id}?as_content_reviewer=yes");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "unparseable flag: served locally, byte for byte"
    );
}

/// [D-150] again: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the mux 404
/// must come from Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/teams/no-pe";

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

/// `partially_migrated` again: only GET is ours, so a DELETE (Go's `deleteTeam`) must reach Go —
/// and its side effect proves it ran there: the team comes back archived.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "getteamdel").await;

    let response = client
        .delete(format!("{RUST}/api/v4/teams/{team_id}"))
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
        "DELETE is not migrated, so it must be forwarded"
    );
    assert_eq!(
        response.status().as_u16(),
        200,
        "Go's deleteTeam accepts it"
    );

    // And the GET both servers now agree on shows the archive actually happened in Go.
    let path = format!("/api/v4/teams/{team_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(
        parsed["delete_at"].as_i64(),
        Some(0),
        "the DELETE reached Go"
    );
}
