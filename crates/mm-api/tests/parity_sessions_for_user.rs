//! Cross-server parity for `GET /api/v4/users/{user_id}/sessions` with an explicit id — the
//! widening of the `me`-only route, whose own bytes `parity_users_me_sessions` keeps pinning.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_sessions_for_user
//! ```
//!
//! The gate is `SessionHasPermissionToUser`, answered as `edit_other_users`: an admin reads
//! anyone, a plain user reads only itself, and the refusal is a 403 whether or not the target
//! exists.

mod common;

use common::{
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, delete_plain_user,
    fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures, stack_enabled,
};

/// An admin reading a plain user's sessions: the `manage_system` branch of the gate, and a
/// list that is not the caller's own. The plain user's one session is idle after login, so its
/// `last_activity_at` does not move under the comparison.
#[tokio::test]
async fn an_admin_reads_another_users_sessions_byte_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let target = create_plain_user(&client, &token, &team_id, "sessadmin").await;

    let path = format!("/api/v4/users/{}/sessions", target.id);
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_plain_user(&client, &token, &target.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert_ne!(rs_body.last(), Some(&b'\n'), "json.Marshal appends nothing");

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let list = parsed.as_array().expect("an array");
    assert!(
        !list.is_empty(),
        "the target logged in, so it has a session"
    );
    for session in list {
        assert_eq!(session["user_id"], target.id.as_str());
        assert_eq!(session["token"], "", "sanitised");
    }
    assert!(
        !String::from_utf8_lossy(&rs_body).contains(&target.token),
        "the target's live token must not be in the body"
    );
}

/// A plain user naming its **own** id explicitly takes the self branch: same answer as `me`.
#[tokio::test]
async fn a_plain_user_reads_its_own_sessions_by_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let me = create_plain_user(&client, &token, &team_id, "sessself").await;

    let by_id = format!("/api/v4/users/{}/sessions", me.id);
    let (go_by_id, rs_by_id) = fetch_both_stable(&client, &me.token, &by_id).await;
    let (_, rs_me) = fetch_both_stable(&client, &me.token, "/api/v4/users/me/sessions").await;
    delete_plain_user(&client, &token, &me.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_by_id),
        String::from_utf8_lossy(&go_by_id)
    );
    assert_eq!(
        rs_by_id, rs_me,
        "`me` and the explicit id are the same request"
    );
}

/// A plain user asking about the admin, and about an id that is nobody: both 403 naming
/// `edit_other_users`, before any `Sessions` read — so the two refusals are indistinguishable.
#[tokio::test]
async fn a_plain_user_is_refused_for_another_user_and_for_nobody_alike() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let viewer = create_plain_user(&client, &token, &team_id, "sessdenied").await;

    let admin_path = format!("/api/v4/users/{}/sessions", common::logged_in_user_id());
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &viewer.token, &admin_path).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "another user");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &viewer.token,
        "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/sessions",
    )
    .await;
    delete_plain_user(&client, &token, &viewer.id).await;
    assert_eq!(go_status, 403, "an unknown target is a 403 too, not a 404");
    assert_eq!(rs_status, 403);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "nobody");
}

/// An admin asking about an id that is nobody: the gate admits it and the list is simply empty.
#[tokio::test]
async fn an_admin_gets_an_empty_array_for_nobody() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &token,
        "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/sessions",
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(rs_body, go_body);
    assert_eq!(rs_body, b"[]", "an empty array, never null");
}

/// A segment that is not an id: the wrong length is `RequireUserId`'s 400 on both servers.
#[tokio::test]
async fn a_short_id_segment_is_a_400_on_both() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/users/tooshort/sessions").await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "short id");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}
