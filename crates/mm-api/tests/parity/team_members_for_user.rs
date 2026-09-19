//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/members` asked about **someone
//! else** — the `me` form has its own suite (`team_members_route.rs`).
//!
//! Every test builds fresh teams and fresh users, so the memberships compared belong to nothing
//! another suite writes. Element order is the store's heap order (no `ORDER BY`), so bodies are
//! compared with the list sorted by `team_id` after a byte comparison fails.
//!
//! ```sh
//! scripts/parity.sh --test parity team_members_for_user
//! ```

use crate::common;

use common::{
    GO, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, fetch_both_raw, go_minted_token, login_plain_user, purge_api_fixtures,
    remove_user_from_team, set_user_roles, stack_enabled,
};

fn path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/members")
}

fn sorted(body: &[u8]) -> Vec<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
    let mut rows = parsed.as_array().expect("an array").clone();
    rows.sort_by_key(|m| m["team_id"].as_str().unwrap_or_default().to_owned());
    rows
}

fn row<'a>(rows: &'a [serde_json::Value], team_id: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|m| m["team_id"] == team_id)
        .unwrap_or_else(|| panic!("no row for team {team_id}"))
}

/// Add an existing user to a team through Go.
async fn join_team(client: &reqwest::Client, admin_token: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "joining {user_id} to {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// `PUT …/schemeRoles` — team admin on this team only.
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

/// A plain user holds neither `edit_other_users` nor `read_other_users_teams`: asking about
/// another plain user is the 403, and asking about themselves by explicit id is served — the
/// self arm of `SessionHasPermissionToUser`, not the fallback permission, admits it.
#[tokio::test]
async fn a_plain_user_sees_their_own_memberships_and_nobody_elses() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "tmfuplain").await;
    let asker = create_plain_user(&client, &admin, &team, "tmfuasker").await;
    let target = create_plain_user(&client, &admin, &team, "tmfutarget").await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &asker.token, &path(&target.id)).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "another user");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &asker.token, &path(&asker.id)).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "one membership, so the order cannot differ"
    );
    assert_eq!(sorted(&rs_body).len(), 1);

    delete_plain_user(&client, &admin, &asker.id).await;
    delete_plain_user(&client, &admin, &target.id).await;
}

/// A system read-only admin reaches another user's memberships through `read_other_users_teams`
/// alone — they lack `edit_other_users`, so `SessionHasPermissionToUser` refuses first. They are
/// also team admin of **one** of the target's two teams, so the sanitiser's per-team guard is live
/// in both directions inside one response: that team's row whole, the other blanked.
///
/// The target's membership in a third team is **removed**, and the soft-deleted row is still
/// listed — `includeDeleted` is `true` and the handler does not filter.
#[tokio::test]
async fn read_other_users_teams_admits_and_sanitising_is_per_team() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin = go_minted_token(&client).await;
    let plain_team = create_team(&client, &admin, "tmfuplainteam").await;
    let admin_team = create_team(&client, &admin, "tmfuadminteam").await;
    let left_team = create_team(&client, &admin, "tmfuleftteam").await;

    let target = create_plain_user(&client, &admin, &plain_team, "tmfuroto").await;
    join_team(&client, &admin, &admin_team, &target.id).await;
    join_team(&client, &admin, &left_team, &target.id).await;
    remove_user_from_team(&client, &admin, &left_team, &target.id).await;

    let reader = create_plain_user(&client, &admin, &admin_team, "tmfureader").await;
    promote_to_team_admin(&client, &admin, &admin_team, &reader.id).await;
    if !set_user_roles(&reader.id, "system_user system_read_only_admin").await {
        eprintln!("skipping: DATABASE_URL is needed to grant the read-only admin role");
        return;
    }
    // Session roles are copied at login; the role grant needs a fresh token.
    let reader_token = login_plain_user(&client, "tmfureader").await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader_token, &path(&target.id)).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_ne!(rs_body.last(), Some(&b'\n'), "json.Marshal, no newline");
    let (go_rows, rs_rows) = (sorted(&go_body), sorted(&rs_body));
    assert_eq!(rs_rows, go_rows);

    // Not vacuous: the three rows each show the rule they exist for.
    assert_eq!(rs_rows.len(), 3, "the removed membership is still listed");
    let managed = row(&rs_rows, &admin_team);
    assert!(!managed["roles"].as_str().unwrap_or_default().is_empty());
    assert_eq!(managed["delete_at"], 0);
    let unmanaged = row(&rs_rows, &plain_team);
    assert_eq!(unmanaged["roles"], "");
    assert_eq!(unmanaged["delete_at"], -1, "the sanitised sentinel");

    // Without the role the reader is a plain user again, and the fallback no longer admits.
    set_user_roles(&reader.id, "system_user").await;
    let plain_token = login_plain_user(&client, "tmfureader").await;
    let ((go_status, _), (rs_status, _)) =
        fetch_both_raw(&client, &plain_token, &path(&target.id)).await;
    assert_eq!((go_status, rs_status), (403, 403));

    // And the system admin sees every row whole, the removed one with its real `delete_at`.
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &admin, &path(&target.id)).await;
    assert_eq!((go_status, rs_status), (200, 200));
    let rs_rows = sorted(&rs_body);
    assert_eq!(rs_rows, sorted(&go_body));
    assert!(
        row(&rs_rows, &left_team)["delete_at"]
            .as_i64()
            .unwrap_or_default()
            > 0
    );

    delete_plain_user(&client, &admin, &reader.id).await;
    delete_plain_user(&client, &admin, &target.id).await;
}

/// `UserCanSeeOtherUser`'s refusal, reached on an unlicensed server: `system_read_only_admin`
/// **without** `system_user` holds `read_other_users_teams` but not `view_members`, so the caller
/// passes the gate and is then view-restricted to its own teams and channels. A target sharing
/// neither is the 403 naming `view_members`; a target on the caller's team is served.
#[tokio::test]
async fn a_view_restricted_reader_sees_only_users_it_shares_a_team_with() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin = go_minted_token(&client).await;
    let shared_team = create_team(&client, &admin, "tmfusharedteam").await;
    let other_team = create_team(&client, &admin, "tmfuotherteam").await;

    let reader = create_plain_user(&client, &admin, &shared_team, "tmfurestricted").await;
    let visible = create_plain_user(&client, &admin, &shared_team, "tmfuvisible").await;
    let hidden = create_plain_user(&client, &admin, &other_team, "tmfuhidden").await;
    if !set_user_roles(&reader.id, "system_read_only_admin").await {
        eprintln!("skipping: DATABASE_URL is needed to set the reader's roles");
        return;
    }
    let reader_token = login_plain_user(&client, "tmfurestricted").await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader_token, &path(&hidden.id)).await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "hidden target");

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader_token, &path(&visible.id)).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(sorted(&rs_body), sorted(&go_body));

    delete_plain_user(&client, &admin, &reader.id).await;
    delete_plain_user(&client, &admin, &visible.id).await;
    delete_plain_user(&client, &admin, &hidden.id).await;
}

/// `RequireUserId` runs before any permission check: an alphanumeric id of the wrong length is
/// Go's 400. (A hyphen would fall outside Go's mux charset — Go's 404, forwarded.)
#[tokio::test]
async fn a_malformed_user_id_is_a_400_on_both() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let admin = go_minted_token(&client).await;
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &admin, &path("notanid")).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "malformed id");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}
