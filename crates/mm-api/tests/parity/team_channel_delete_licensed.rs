//! `DELETE /api/v4/teams/{team_id}`, `DELETE /api/v4/channels/{channel_id}`,
//! `GET /api/v4/teams` and `POST /api/v4/teams/search` on the **licensed** pair — [D-371].
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity team_channel_delete_licensed
//! ```
//!
//! Until 2026-09-13 both deletes forwarded a licensed installation because
//! `cleanupTeamAccessControlPolicy` / `cleanupChannelAccessControlPolicy` run between the write
//! and the event, on the belief that they needed the enterprise access-control service. They do
//! not: the service is nil off the enterprise tree and the store fallback is the whole function
//! (`AccessControlPolicyStore.Delete`, a no-op with no policy row). So the archive is served here,
//! and this suite measures it against the Enterprise-licensed oracle. The store method itself is
//! held down by `mm-store`'s `db_access_control_policy` test, which plants a policy row.
//!
//! The two listings are here for the other half of the entry: `TeamMembershipAccessControlEnabled`
//! needs Enterprise **Advanced** and the ABAC setting, so on this pair it is false and both are
//! served in full; with it true they forward. This measures the served half.

use crate::common;

use common::{
    a_team_and_channel_the_user_is_in, client, create_channel, create_team, go_minted_token,
    invalidate_licensed_go_caches, licensed, request_raw, stack_enabled,
};

/// Plant an access-control policy row carrying `id`, so an archive has something to clean up.
///
/// Nothing over HTTP can write one below Enterprise Advanced; this is the only way the
/// copy-then-delete arm of `cleanupTeamAccessControlPolicy` is reachable on this stack.
async fn plant_policy(id: &str) -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set under the harness");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    for table in ["accesscontrolpolicies", "accesscontrolpolicyhistory"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO accesscontrolpolicies (id, name, type, active, createat, revision, version, data, props)
         VALUES ($1, 'mmrs archive fixture', 'team', true, 1767225600000, 1, 'v0.2', '{}'::jsonb, '{}'::jsonb)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("plants a policy row");
    pool
}

/// `(live rows, history rows)` for `id`.
async fn policy_rows(pool: &sqlx::PgPool, id: &str) -> (i64, i64) {
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accesscontrolpolicies WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accesscontrolpolicyhistory WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    (live, history)
}

async fn clear_policy(pool: &sqlx::PgPool, id: &str) {
    for table in ["accesscontrolpolicies", "accesscontrolpolicyhistory"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// Archive one team through each licensed server; both answer `ReturnStatusOK` and both teams
/// read back archived from the oracle.
#[tokio::test]
async fn a_team_archive_is_served_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let ours = create_team(&client, &admin, "ltdrs").await;
    let theirs = create_team(&client, &admin, "ltdgo").await;
    // Created through the stack's Go server: the oracle's team cache has not seen them.
    invalidate_licensed_go_caches(&client, &pair, &admin).await;
    // A policy row on each, so `cleanupTeamAccessControlPolicy`'s store fallback has work to do.
    let pool = plant_policy(&ours).await;
    plant_policy(&theirs).await;

    let (rs_status, rs_body, served) = request_raw(
        &client,
        &pair.rust,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/teams/{ours}"),
        None,
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"), "served, not forwarded");
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (go_status, go_body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/teams/{theirs}"),
        None,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_body, go_body, "ReturnStatusOK on both, no newline");

    for (team, who) in [(&ours, "ours"), (&theirs, "Go's")] {
        assert_eq!(
            policy_rows(&pool, team).await,
            (0, 1),
            "{who}: the policy row moved to history on archive"
        );
        clear_policy(&pool, team).await;
    }

    invalidate_licensed_go_caches(&client, &pair, &admin).await;
    for (team, who) in [(&ours, "ours"), (&theirs, "Go's")] {
        let (status, body, _) = request_raw(
            &client,
            &pair.go,
            reqwest::Method::GET,
            Some(&admin),
            &format!("/api/v4/teams/{team}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{who}: {}", String::from_utf8_lossy(&body));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value["delete_at"].as_i64().unwrap_or(0) > 0,
            "{who}: archived, as the oracle reads it: {value}"
        );
    }
}

/// Archive one channel through each licensed server, the same way.
#[tokio::test]
async fn a_channel_archive_is_served_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let ours = create_channel(&client, &admin, &team, "lcdrs").await;
    let theirs = create_channel(&client, &admin, &team, "lcdgo").await;
    invalidate_licensed_go_caches(&client, &pair, &admin).await;
    let pool = plant_policy(&ours).await;
    plant_policy(&theirs).await;

    let (rs_status, rs_body, served) = request_raw(
        &client,
        &pair.rust,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/channels/{ours}"),
        None,
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"), "served, not forwarded");
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (go_status, go_body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::DELETE,
        Some(&admin),
        &format!("/api/v4/channels/{theirs}"),
        None,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_body, go_body);

    for (channel, who) in [(&ours, "ours"), (&theirs, "Go's")] {
        assert_eq!(
            policy_rows(&pool, channel).await,
            (0, 1),
            "{who}: the policy row moved to history on archive"
        );
        clear_policy(&pool, channel).await;
    }

    invalidate_licensed_go_caches(&client, &pair, &admin).await;
    for (channel, who) in [(&ours, "ours"), (&theirs, "Go's")] {
        let (status, body, _) = request_raw(
            &client,
            &pair.go,
            reqwest::Method::GET,
            Some(&admin),
            &format!("/api/v4/channels/{channel}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{who}: {}", String::from_utf8_lossy(&body));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value["delete_at"].as_i64().unwrap_or(0) > 0,
            "{who}: archived, as the oracle reads it: {value}"
        );
    }
}

/// `GET /teams` and `POST /teams/search` are served in full on an Enterprise licence — the ABAC
/// gate needs Advanced — and match the oracle byte for byte.
#[tokio::test]
async fn the_two_team_listings_are_served_and_identical_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "ltlsrch").await;
    invalidate_licensed_go_caches(&client, &pair, &admin).await;

    // The search is scoped to this test's own term, so its two answers are byte-comparable.
    let search = serde_json::json!({ "term": "mmrs-parity-ltlsrch" }).to_string();
    let path = "/api/v4/teams/search";
    let (go_status, go_body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::POST,
        Some(&admin),
        path,
        Some(search.as_bytes()),
    )
    .await;
    let (rs_status, rs_body, served) = request_raw(
        &client,
        &pair.rust,
        reqwest::Method::POST,
        Some(&admin),
        path,
        Some(search.as_bytes()),
    )
    .await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path}: served, not forwarded"
    );
    assert_eq!(
        go_status,
        200,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "{path}: byte for byte"
    );
    assert!(
        String::from_utf8_lossy(&go_body).contains(&team),
        "{path}: the fixture team is found"
    );

    // The whole listing is every suite's teams, created and archived under this test, so only
    // this test's own row is compared — the sibling archive test moved a team between the two
    // requests on the first run and the listings disagreed by exactly that team.
    let path = "/api/v4/teams?per_page=200";
    let fixture_row = |body: &[u8]| -> serde_json::Value {
        let teams: Vec<serde_json::Value> = serde_json::from_slice(body).expect("a team array");
        teams
            .into_iter()
            .find(|t| t["id"] == team)
            .unwrap_or_else(|| panic!("{path}: the fixture team is listed"))
    };
    let (go_status, go_body, _) = request_raw(
        &client,
        &pair.go,
        reqwest::Method::GET,
        Some(&admin),
        path,
        None,
    )
    .await;
    let (rs_status, rs_body, served) = request_raw(
        &client,
        &pair.rust,
        reqwest::Method::GET,
        Some(&admin),
        path,
        None,
    )
    .await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path}: served, not forwarded"
    );
    assert_eq!(
        go_status,
        200,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        fixture_row(&rs_body),
        fixture_row(&go_body),
        "{path}: the fixture team's row"
    );
}
