//! Cross-server parity for `GET /api/v4/channels/{channel_id}/members/{user_id}`.
//!
//! The first route served from Rust whose **only** gate is a real permission check —
//! `SessionHasPermissionToChannel` — rather than one that provably cannot change the answer. So
//! these tests compare refusals as carefully as successes: a route that answers correctly and
//! refuses incorrectly is not migrated, it is broken.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_channel_member
//! ```
//!
//! # Every fixture here is created by this file, not borrowed from the database
//!
//! An earlier version read whatever channel the fixture user happened to be in. Running the file
//! end to end then removed that user from one of the development database's two channels — no
//! single test did it, only the combination, and it went unnoticed for a session because nothing
//! was watching. The lesson is not "find the guilty test": it is that a suite which mutates rows
//! it did not create has no business asserting anything about them. Channels and second users are
//! made here and unwound here; [`common::purge_api_fixtures`] clears what a panicking run leaves.

mod common;

use common::{
    a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, delete_channel, delete_plain_user, fetch_both_raw,
    fetch_both_stable, go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

/// Create a channel on the fixture user's team. The creator is automatically a member, so this
/// alone gives the caller a membership row to read.
async fn own_channel(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, token).await;
    create_channel(client, token, &team_id, tag).await
}

#[tokio::test]
async fn the_member_body_is_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let channel_id = own_channel(&client, &token, "body").await;
    let path = format!(
        "/api/v4/channels/{channel_id}/members/{}",
        logged_in_user_id()
    );

    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &channel_id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    // Not vacuous: assert the body is a populated member, and that the newline this call site
    // owes is actually there ([D-086]).
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["channel_id"].as_str(), Some(channel_id.as_str()));
    assert_eq!(parsed["user_id"].as_str(), Some(logged_in_user_id()));
    assert!(
        !parsed["roles"].as_str().unwrap_or_default().is_empty(),
        "the member holds no roles, so the interesting half of the response is empty"
    );
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "this handler uses an encoder, so the body ends in a newline"
    );
}

/// `me` is resolved to the session's own id **before** the id is validated (web/context.go:301),
/// so the alias must produce exactly the same bytes as the explicit id — on both servers.
#[tokio::test]
async fn the_me_alias_answers_the_same_as_the_explicit_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let channel_id = own_channel(&client, &token, "alias").await;

    let (go_explicit, rs_explicit) = fetch_both_stable(
        &client,
        &token,
        &format!(
            "/api/v4/channels/{channel_id}/members/{}",
            logged_in_user_id()
        ),
    )
    .await;
    let (go_me, rs_me) = fetch_both_stable(
        &client,
        &token,
        &format!("/api/v4/channels/{channel_id}/members/me"),
    )
    .await;
    delete_channel(&client, &token, &channel_id).await;

    assert_eq!(go_me, go_explicit, "Go treats `me` as the session's id");
    assert_eq!(rs_me, rs_explicit, "and so must we");
    assert_eq!(rs_me, go_me);
}

/// `SanitizeForCurrentUser` blanks **another** member's two timestamps to `-1`, and for a long
/// while nothing exercised it: every other test here reads the caller's own membership, for which
/// the sanitiser is a no-op. A mutation deleting the call survived the whole suite.
#[tokio::test]
async fn another_members_timestamps_are_sanitised_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "sanitise").await;
    let plain = create_plain_user(&client, &token, &team_id, "san").await;

    let added = client
        .post(format!(
            "{}/api/v4/channels/{channel_id}/members",
            common::GO
        ))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "user_id": plain.id }))
        .send()
        .await
        .expect("Go answers");
    let added_ok = added.status().is_success();

    let fetched = fetch_both_stable(
        &client,
        &token,
        &format!("/api/v4/channels/{channel_id}/members/{}", plain.id),
    )
    .await;

    delete_plain_user(&client, &token, &plain.id).await;
    delete_channel(&client, &token, &channel_id).await;

    assert!(added_ok, "adding the second member failed");
    let (go_body, rs_body) = fetched;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "another member's row must be byte-identical too"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["user_id"].as_str(), Some(plain.id.as_str()));
    assert_eq!(
        parsed["last_viewed_at"].as_i64(),
        Some(-1),
        "the sentinel is -1, not 0 — and this is the assertion the sanitiser mutation escaped"
    );
    assert_eq!(parsed["last_update_at"].as_i64(), Some(-1));
}

/// **The permission this route asks for actually matters** — asserted with an actor who can be
/// refused.
///
/// Every other test here runs as the fixture user, a `system_admin`. Branch 5 of
/// `SessionHasPermissionToChannel` grants on `manage_system` *whatever* permission was asked for,
/// so for that user the handler could name any permission and every test would still pass. A
/// mutation swapping `PermissionReadChannel` for `PermissionManageSystem` survived the whole
/// suite, which is how this test came to exist.
///
/// The plain user is in the team but joins only one of two channels. Membership grants
/// (`channel_user` carries `read_channel`); the team fallback does not (`team_user` does not), so
/// the second channel refuses. Both halves are compared against Go.
#[tokio::test]
async fn a_non_admin_is_granted_by_membership_and_refused_without_it() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin_token).await;
    let joined = create_channel(&client, &admin_token, &team_id, "joined").await;
    let not_joined = create_channel(&client, &admin_token, &team_id, "notjoined").await;
    let plain = create_plain_user(&client, &admin_token, &team_id, "cm").await;

    let added = client
        .post(format!("{}/api/v4/channels/{joined}/members", common::GO))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "user_id": plain.id }))
        .send()
        .await
        .expect("Go answers");
    let added_ok = added.status().is_success();

    let granted = fetch_both_stable(
        &client,
        &plain.token,
        &format!("/api/v4/channels/{joined}/members/{}", plain.id),
    )
    .await;
    let refused = fetch_both_raw(
        &client,
        &plain.token,
        &format!("/api/v4/channels/{not_joined}/members/{}", plain.id),
    )
    .await;

    delete_plain_user(&client, &admin_token, &plain.id).await;
    delete_channel(&client, &admin_token, &joined).await;
    delete_channel(&client, &admin_token, &not_joined).await;

    assert!(added_ok, "adding the plain user to the channel failed");

    let (go_body, rs_body) = granted;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "membership grants, and the two bodies must match"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["roles"].as_str(),
        Some("channel_user"),
        "the grant must come from channel membership, not from a system role"
    );

    let ((go_status, go_body), (rs_status, rs_body)) = refused;
    assert_eq!(
        go_status, 403,
        "a team member who is not in the channel must be refused — if this is 200, the team's \
         roles grant read_channel and this test proves nothing"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "non-member refusal");
}

/// A channel that does not exist is a **403**, not a 404 — the permission check runs first, its
/// `GetChannel` misses, and it denies. Leaking the difference would tell a caller which channel
/// ids exist.
#[tokio::test]
async fn a_missing_channel_refuses_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!(
        "/api/v4/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/members/{}",
        logged_in_user_id()
    );

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;

    assert_eq!(go_status, 403, "Go denies rather than reporting a 404");
    assert_eq!(rs_status, go_status);

    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing channel");
    assert_eq!(
        go["id"].as_str(),
        Some("api.context.permissions.app_error"),
        "the refusal must be the permission error, not a not-found"
    );
}

/// A path segment that is not an id is a 400 naming the parameter.
///
/// The *ordering* claim — channel before user — rests on Go's translated message here, because
/// `AppError` does not serialise `params` and our own message is still the raw id ([D-092]). The
/// order is pinned on our side by a unit test in `channels.rs` instead.
#[tokio::test]
async fn malformed_ids_refuse_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    for (path, expected_param) in [
        (
            format!("/api/v4/channels/nope/members/{}", logged_in_user_id()),
            "channel_id",
        ),
        (
            "/api/v4/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/members/nope".to_owned(),
            "user_id",
        ),
        // Both malformed: Go reports the channel, because it is checked first.
        (
            "/api/v4/channels/nope/members/alsonope".to_owned(),
            "channel_id",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;

        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");

        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_url_param.app_error"),
            "{path}"
        );
        assert!(
            go["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_param),
            "{path}: expected Go to name {expected_param}, got {}",
            go["message"]
        );
    }
}

/// Unmigrated methods on this path still reach Go. `partially_migrated` is what makes that true,
/// and forgetting it turns a working proxied route into a 405 from our own router ([D-093]).
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let channel_id = own_channel(&client, &token, "forward").await;

    let response = client
        .delete(format!(
            "{}/api/v4/channels/{channel_id}/members/{}",
            common::RUST,
            logged_in_user_id()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server is reachable");
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    delete_channel(&client, &token, &channel_id).await;

    assert_ne!(
        status, 405,
        "a 405 here means the path was registered without a proxy fallback"
    );
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "DELETE is unmigrated and must be forwarded"
    );
}
