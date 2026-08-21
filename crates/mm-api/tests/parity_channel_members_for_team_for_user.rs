//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/channels/members` — the
//! request the webapp sends right after the channel list on every team load.
//!
//! Every test builds a **fresh team**, for the reason its sibling suite
//! (`parity_channels_for_team_for_user.rs`) gives: the shared fixture team is written to by every
//! other suite, and a list is only comparable when its membership holds still. DMs are teamless
//! and appear under every team, so `fetch_both_stable` absorbs a DM another suite opens with the
//! fixture user mid-test.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_channel_members_for_team_for_user
//! ```

mod common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
    view_channel,
};

/// Create a team through Go's API and return its id. The creator joins automatically and gets
/// `town-square` and `off-topic`.
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

async fn create_typed_channel(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
    channel_type: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": channel_type,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn open_dm(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success());
    let created: serde_json::Value = response.json().await.expect("the DM decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Make `user_id` a team admin of `team_id` (`PUT …/schemeRoles`, api4/team.go:70) — a caller
/// with `manage_team` and without `manage_system`, the pair the second gate tells apart.
async fn promote_to_team_admin(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    user_id: &str,
) {
    let response = client
        .put(format!(
            "{GO}/api/v4/teams/{team_id}/members/{user_id}/schemeRoles"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "scheme_user": true, "scheme_admin": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "promoting {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn rows(body: &[u8]) -> Vec<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
    parsed.as_array().expect("an array").clone()
}

/// The caller's own memberships, byte for byte through `me` and the explicit id: an open and a
/// private channel, a DM (teamless, listed under this team), a viewed channel so a timestamp is
/// non-zero and visibly **not** blanked, a root post and a reply so the two message counters
/// differ, and an **archived** channel whose membership is still listed — the one rule the
/// sibling channel list inverts.
#[tokio::test]
async fn own_memberships_are_byte_identical_and_unsanitized() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "memteam").await;

    let open = create_typed_channel(&client, &token, &team_id, "memopen", "O").await;
    let private = create_typed_channel(&client, &token, &team_id, "mempriv", "P").await;
    let archived = create_typed_channel(&client, &token, &team_id, "memarch", "O").await;
    let other = create_plain_user(&client, &token, &team_id, "memdm").await;
    let dm = open_dm(&client, &token, logged_in_user_id(), &other.id).await;

    let root = post_message(&client, &token, &open, "a root post", None).await;
    post_message(&client, &token, &open, "a reply", Some(&root)).await;
    view_channel(&client, &token, &open).await;
    delete_channel(&client, &token, &archived).await;

    let me = logged_in_user_id();
    for path in [
        format!("/api/v4/users/me/teams/{team_id}/channels/members"),
        format!("/api/v4/users/{me}/teams/{team_id}/channels/members"),
    ] {
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{path}: the two servers must agree byte for byte"
        );
        assert_eq!(rs_body.last(), Some(&b'\n'), "encoder newline ([D-086])");

        let list = rows(&rs_body);
        assert!(list.iter().all(|m| m["user_id"] == me), "every row is mine");
        for (id, why) in [
            (&open, "the open channel"),
            (&private, "the private channel"),
            (&dm, "the DM — teamless — appears in this team's list"),
            (
                &archived,
                "an archived channel's membership is still listed",
            ),
        ] {
            assert!(
                list.iter().any(|m| m["channel_id"] == id.as_str()),
                "{path}: {why}"
            );
        }
        let open_row = list
            .iter()
            .find(|m| m["channel_id"] == open.as_str())
            .expect("the open channel is listed");
        assert!(
            open_row["last_viewed_at"].as_i64().unwrap_or_default() > 0,
            "a self read is not sanitized"
        );
        assert_ne!(
            open_row["msg_count"], open_row["msg_count_root"],
            "equal counters could not catch a swapped column"
        );
    }

    for id in [&open, &private] {
        delete_channel(&client, &token, id).await;
    }
    delete_plain_user(&client, &token, &other.id).await;
}

/// An admin reading **another user's** memberships: byte-identical, and because every row is
/// the target's, `SanitizeForCurrentUser` blanks `last_viewed_at` and `last_update_at` to `-1`
/// on every one of them.
#[tokio::test]
async fn an_admin_reading_another_user_gets_every_row_sanitized() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "adminmem").await;
    let target = create_plain_user(&client, &token, &team_id, "admintarget").await;
    let channel = create_typed_channel(&client, &token, &team_id, "adminchan", "O").await;
    add_user_to_channel(&client, &token, &channel, &target.id).await;
    view_channel(&client, &target.token, &channel).await;

    let path = format!(
        "/api/v4/users/{}/teams/{team_id}/channels/members",
        target.id
    );
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );

    let list = rows(&rs_body);
    assert!(
        list.iter().any(|m| m["channel_id"] == channel.as_str()),
        "the target's channel is listed"
    );
    assert!(!list.is_empty());
    for row in &list {
        assert_eq!(row["user_id"], target.id.as_str());
        assert_eq!(row["last_viewed_at"], -1, "sanitized for the admin");
        assert_eq!(row["last_update_at"], -1, "sanitized for the admin");
    }

    delete_channel(&client, &token, &channel).await;
    delete_plain_user(&client, &token, &target.id).await;
}

/// Zero memberships is **`[]`**, not the sibling list's 404: the admin passes both gates for a
/// team the target never joined.
#[tokio::test]
async fn a_user_with_no_memberships_in_the_team_is_an_empty_array() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let stranger = create_plain_user(&client, &token, &home_team, "memstranger").await;
    let team_id = create_team(&client, &token, "memempty").await;

    let path = format!(
        "/api/v4/users/{}/teams/{team_id}/channels/members",
        stranger.id
    );
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    delete_plain_user(&client, &token, &stranger.id).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(String::from_utf8_lossy(&go_body), "[]\n");
    assert_eq!(String::from_utf8_lossy(&rs_body), "[]\n");
}

/// The two gates with an actor who can be refused: asking about oneself in a team one is **not a
/// member of** fails the team gate (`view_team`); asking about **another user** in one's own
/// team passes it and fails the `manage_system` check. Both are the same 403 body over HTTP; the
/// order is visible only in which one fires for a plain user asking about another user in a
/// foreign team — `view_team`, because the team gate runs first.
#[tokio::test]
async fn a_plain_user_is_refused_by_both_gates() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let plain = create_plain_user(&client, &token, &home_team, "memgates").await;
    let foreign_team = create_team(&client, &token, "memforeign").await;

    let admin = logged_in_user_id();
    for (path, why) in [
        (
            format!("/api/v4/users/me/teams/{foreign_team}/channels/members"),
            "a team not joined: view_team",
        ),
        (
            format!("/api/v4/users/{admin}/teams/{home_team}/channels/members"),
            "another user: manage_system",
        ),
        (
            format!("/api/v4/users/{admin}/teams/{foreign_team}/channels/members"),
            "another user in a foreign team: view_team first",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &plain.token, &path).await;
        assert_eq!(go_status, 403, "{why}");
        assert_eq!(rs_status, 403, "{why}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, why);
        assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
    }

    delete_plain_user(&client, &token, &plain.id).await;
}

/// The second gate asks for `manage_system`, not `manage_team`: a **team admin** reading another
/// member's memberships in their own team is refused on both servers. `manage_team` is the
/// plausible wrong constant — it admits a team admin to the sibling by-name route — and a plain
/// user cannot tell the two apart, so this caller exists to.
#[tokio::test]
async fn a_team_admin_is_refused_for_another_user() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "teamadmin").await;
    let team_admin = create_plain_user(&client, &token, &team_id, "memteamadmin").await;
    promote_to_team_admin(&client, &token, &team_id, &team_admin.id).await;

    let admin = logged_in_user_id();
    let path = format!("/api/v4/users/{admin}/teams/{team_id}/channels/members");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &team_admin.token, &path).await;
    assert_eq!(go_status, 403, "a team admin lacks manage_system");
    assert_eq!(rs_status, 403, "a team admin lacks manage_system");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "team admin");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));

    // And their own memberships are served — the team gate passed, the self shortcut fired.
    let own = format!("/api/v4/users/me/teams/{team_id}/channels/members");
    let (go_body, rs_body) = fetch_both_stable(&client, &team_admin.token, &own).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(!rows(&rs_body).is_empty(), "town-square at least");

    delete_plain_user(&client, &token, &team_admin.id).await;
}

/// A malformed id in either segment is `invalid_url_param`, and Go's `RequireUserId` handles
/// `me` before validating, so the literal never 400s.
#[tokio::test]
async fn malformed_ids_are_400s() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for path in [
        format!("/api/v4/users/short/teams/{team_id}/channels/members"),
        "/api/v4/users/me/teams/short/channels/members".to_owned(),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, 400, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_url_param.app_error")
        );
    }
}

/// The route is served here, not forwarded — and the `categories` sibling still is forwarded.
#[tokio::test]
async fn the_route_is_served_by_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for (suffix, served_by) in [("members", "rust"), ("categories", "go")] {
        let path = format!("/api/v4/users/me/teams/{team_id}/channels/{suffix}");
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
            Some(served_by),
            "{suffix}"
        );
        assert_eq!(response.status().as_u16(), 200, "{suffix}");
    }
}
