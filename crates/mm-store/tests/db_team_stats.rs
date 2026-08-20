//! The two `getTeamStats` count queries against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_stats
//! ```
//!
//! The one distinction the whole route hangs on: **"total" is current memberships including
//! deactivated users**. `TeamMembers.DeleteAt = 0` filters departures from *both* counts, and
//! only the active count adds `Users.DeleteAt = 0` — so a departed member counts in neither, a
//! deactivated user's surviving membership counts in the total only. Each shape gets a row here,
//! plus an other-team member for the team-id predicate; the parity suite drives the same split
//! over REST with Go as the oracle.

use mm_store::team_store::{get_active_member_count, get_total_member_count};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Same file-local serialisation as every other DB suite.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrststeam000000000000main";
const OTHER_TEAM: &str = "mmrststeam00000000000other";
const USER_ACTIVE: &str = "mmrstsuser000000000000actv";
const USER_DEACTIVATED_1: &str = "mmrstsuser00000000000deac1";
const USER_DEACTIVATED_2: &str = "mmrstsuser00000000000deac2";
const USER_DEPARTED: &str = "mmrstsuser000000000000left";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrsts%' OR userid LIKE 'mmrsts%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsts%'",
        "DELETE FROM users WHERE id LIKE 'mmrsts%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn insert_user(pool: &PgPool, id: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, email, roles, lastlogin)
         VALUES ($1, 0, 0, $2, $1, $1 || '@mmrs.invalid', 'system_user', 0)",
    )
    .bind(id)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the user");
}

async fn insert_member(pool: &PgPool, team_id: &str, user_id: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin, schemeguest)
         VALUES ($1, $2, '', $3, true, false, false)",
    )
    .bind(team_id)
    .bind(user_id)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

/// total = 3 (one active, two deactivated users with live rows), active = 1; the departed
/// member and the other team's member count in neither.
async fn seed(pool: &PgPool) {
    for id in [TEAM, OTHER_TEAM] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs team stats', $1, 'O', false)",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    insert_user(pool, USER_ACTIVE, 0).await;
    insert_user(pool, USER_DEACTIVATED_1, 1_700_000_000_000).await;
    insert_user(pool, USER_DEACTIVATED_2, 1_700_000_000_000).await;
    insert_user(pool, USER_DEPARTED, 0).await;

    insert_member(pool, TEAM, USER_ACTIVE, 0).await;
    insert_member(pool, TEAM, USER_DEACTIVATED_1, 0).await;
    insert_member(pool, TEAM, USER_DEACTIVATED_2, 0).await;
    insert_member(pool, TEAM, USER_DEPARTED, 1_700_000_000_000).await; // departed
    insert_member(pool, OTHER_TEAM, USER_ACTIVE, 0).await; // other team's member
}

#[tokio::test]
async fn total_counts_deactivated_users_and_active_does_not() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let total = get_total_member_count(&pool, TEAM).await.expect("counts");
    let active = get_active_member_count(&pool, TEAM).await.expect("counts");

    assert_eq!(
        total, 3,
        "the two deactivated users' surviving rows count; the departed member's row does not"
    );
    assert_eq!(active, 1, "only the active user survives both predicates");
    assert_ne!(
        total, active,
        "equal counts could not catch the missing Users.DeleteAt predicate"
    );

    purge(&pool).await;
}

/// A well-formed id that matches nothing is two zeroes — which `getTeamStats` serves as a 200
/// to any caller its gate admits, since nothing in the handler fetches the team.
#[tokio::test]
async fn a_missing_team_counts_zero_in_both() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let missing = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    assert_eq!(
        get_total_member_count(&pool, missing)
            .await
            .expect("counts"),
        0
    );
    assert_eq!(
        get_active_member_count(&pool, missing)
            .await
            .expect("counts"),
        0
    );

    purge(&pool).await;
}
