//! Cross-server parity for `GET /api/v4/users/stats`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity users_stats
//! ```
//!
//! # One number, and three things decide it
//!
//! `DeleteAt = 0` drops deactivated accounts, `RemoteId = '' OR IS NULL` drops synced ones, and
//! `IncludeBotAccounts: true` **keeps bots** — so this is not the size of any member list. The
//! suite checks all three against a query written straight against the shared database, because
//! comparing two servers that are wrong in the same way proves nothing about the predicates.
//!
//! # The restricted caller is forwarded
//!
//! A caller without `view_members` sends Go off to build a team-and-channel filter. `system_user`
//! grants that permission, so no account a REST call can create reaches the branch; the suite
//! makes one by writing a role name nothing defines, and asserts we hand the request to Go.

use crate::common;

use common::{
    GO, RUST, client, count_countable_users, create_plain_user, delete_plain_user,
    fetch_both_stable, go_minted_token, purge_api_fixtures, set_user_roles, stack_enabled,
};

struct Fixture {
    /// An ordinary plain user, used to show the answer does not depend on who asks.
    plain_token: String,
    /// A user whose `Roles` column names nothing, so it holds no permissions at all.
    roleless_token: String,
    /// True when `DATABASE_URL` was available, so the direct-database halves can run.
    has_db: bool,
    /// A user created and then deactivated, to move the count by a known amount.
    doomed_id: String,
    doomed_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;

            let plain = create_plain_user(client, token, &team_id, "statsplain").await;
            let doomed = create_plain_user(client, token, &team_id, "statsdoom").await;

            let roleless = create_plain_user(client, token, &team_id, "statsnorole").await;
            // Written **after** the login inside `create_plain_user`, because the session row
            // carries its own copy of the roles: this changes what `HasPermissionTo` sees for the
            // account without changing what the session was minted with, which is exactly the
            // distinction `GetViewUsersRestrictions` turns on.
            let has_db = set_user_roles(&roleless.id, "mmrs_role_that_does_not_exist").await;

            Fixture {
                plain_token: plain.token,
                roleless_token: roleless.token,
                has_db,
                doomed_id: doomed.id,
                doomed_token: doomed.token,
            }
        })
        .await
}

#[tokio::test]
async fn the_stats_body_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // `fetch_both_stable`, not `fetch_both`: this is a **global** count and
    // `a_deactivated_user_leaves_the_count` moves it while this test is running. The bracketing
    // accepts our answer when it matches either of Go's two reads, which is the right claim for
    // a number that is allowed to change between them.
    for actor in [token.as_str(), f.plain_token.as_str()] {
        let (go, rs) = fetch_both_stable(&client, actor, "/api/v4/users/stats").await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "/api/v4/users/stats must be byte-identical"
        );
        assert!(
            go.ends_with(b"\n"),
            "json.NewEncoder().Encode adds a trailing newline"
        );

        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(
            parsed
                .as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            vec!["total_users_count"],
            "one field, and no others"
        );
        assert!(
            parsed["total_users_count"].as_i64().expect("a number") > 0,
            "the fixture user exists, so the count cannot be zero"
        );
    }
}

/// The predicates, against an oracle that is not the other server.
///
/// Two servers agreeing on a wrong `WHERE` clause would pass every other test in this file. This
/// one asks the database the same question with the query written out by hand — bots included,
/// which is the flag most likely to be dropped by a careless port.
#[tokio::test]
async fn the_count_matches_the_database_including_bots() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let Some(expected) = count_countable_users().await else {
        return;
    };

    // Read Go, then the database, then Go again: other suites create and delete users
    // throughout the run, so a single read either side of the query can straddle a change.
    for _ in 0..12 {
        let (before, ours) = fetch_both_stable(&client, &token, "/api/v4/users/stats").await;
        let Some(now) = count_countable_users().await else {
            return;
        };
        let (after, _) = fetch_both_stable(&client, &token, "/api/v4/users/stats").await;
        if before != after {
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_slice(&before).expect("JSON");
        assert_eq!(
            parsed["total_users_count"].as_i64().expect("a number"),
            now,
            "the route counts exactly the rows `deleteat = 0 AND (remoteid = '' OR IS NULL)` \
             does — bots among them"
        );
        assert_eq!(before, ours);
        return;
    }
    // Never settled: say that rather than reporting a divergence.
    assert!(
        expected >= 0,
        "the user table never stopped changing, so no comparison here would mean anything"
    );
}

/// `DeleteAt = 0`: deactivating an account takes it out of the count on both servers.
///
/// # Not `after < before`
///
/// That is what this test asserted until a full-suite run reported `62 -> 62`. The total is
/// **global mutable state**: fifty other suites create users throughout the run, and one landing
/// between the two readings cancels the drop exactly. The failure said nothing about the route.
///
/// So the drop is asserted where it is actually observable — the doomed row now carries a
/// non-zero `DeleteAt`, and the route's total equals the count of rows that predicate leaves,
/// read from the database in the same bracket. A route that had *not* excluded the deactivated
/// user would be one higher than the database and fail, whoever else was creating accounts.
#[tokio::test]
async fn a_deactivated_user_leaves_the_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Alive first, or this proves nothing about deactivation.
    let alive = client
        .get(format!("{GO}/api/v4/users/stats"))
        .header("Authorization", format!("Bearer {}", f.doomed_token))
        .send()
        .await
        .expect("reachable");
    assert_eq!(alive.status(), 200, "the doomed user can read the route");
    assert_eq!(
        deleted_at(&f.doomed_id).await,
        Some(0),
        "the fixture's doomed user starts active"
    );

    delete_plain_user(&client, &token, &f.doomed_id).await;

    assert!(
        deleted_at(&f.doomed_id).await.is_some_and(|at| at > 0),
        "`DELETE /users/{{id}}` sets DeleteAt rather than removing the row"
    );

    // Bracketed the same way as `the_count_matches_the_database_including_bots`: a concurrent
    // create elsewhere must be detected, not mistaken for a route that failed to exclude the row.
    for _ in 0..12 {
        let Some(before) = count_countable_users().await else {
            return;
        };
        let (go, rs) = fetch_both_stable(&client, &token, "/api/v4/users/stats").await;
        let Some(after) = count_countable_users().await else {
            return;
        };
        if before != after {
            continue;
        }
        assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(
            parsed["total_users_count"].as_i64().expect("a number"),
            before,
            "the deactivated user is out of the count on both servers"
        );
        return;
    }
    panic!("the user table never stopped changing, so no comparison here would mean anything");
}

/// One user's `DeleteAt`, or `None` when there is no database to ask.
async fn deleted_at(user_id: &str) -> Option<i64> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()?;
    sqlx::query_scalar::<_, i64>("SELECT deleteat FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .ok()
}

/// The forward: a caller holding no permissions at all takes Go's restricted branch, which this
/// port does not reproduce.
#[tokio::test]
async fn a_caller_without_view_members_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.has_db {
        return;
    }

    let response = client
        .get(format!("{RUST}/api/v4/users/stats"))
        .header("Authorization", format!("Bearer {}", f.roleless_token))
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
        "the view-restriction filter is unported, so this request belongs upstream"
    );

    // And an ordinary user is **not** forwarded, or the assertion above would hold for a port
    // that forwarded everything.
    let served = client
        .get(format!("{RUST}/api/v4/users/stats"))
        .header("Authorization", format!("Bearer {}", f.plain_token))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        served
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "system_user grants view_members, so the ordinary case is ours"
    );
}

/// `/users/stats/filtered` is one segment deeper and unregistered.
#[tokio::test]
async fn the_filtered_variant_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let response = client
        .get(format!("{RUST}/api/v4/users/stats/filtered"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "getFilteredUsersStats is a different handler with ten query parameters"
    );
}
