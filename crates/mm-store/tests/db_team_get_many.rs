//! `SqlTeamStore::get_many` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_get_many
//! ```
//!
//! # Two properties no route can reach
//!
//! `GetMany` has **no `DeleteAt` predicate** (team_store.go:373), so it returns archived teams —
//! and it turns an **empty result into `ErrNotFound`** (team_store.go:381) rather than an empty
//! slice, which the app layer answers 404 to. Its only migrated caller,
//! `getDirectOrGroupMessageMembersCommonTeams`, feeds it ids that came from a query which already
//! filtered archived teams out and is never empty — so neither behaviour is observable over HTTP,
//! and a mutation of either survives the whole parity suite.

use mm_store::{SqlTeamStore, TeamStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LIVE: &str = "mmrsgetmany0000000000live1";
const ARCHIVED: &str = "mmrsgetmany00000000archive";
const ABSENT: &str = "mmrsgetmany000000000absent";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM teams WHERE id LIKE 'mmrsgetmany%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// Every column Go's `model.Team` scans into a non-pointer field gets a value — a NULL in one of
/// those is a scan failure for the **Go** server, which reads these same rows.
async fn seed(pool: &PgPool) {
    // **Distinct display names.** `parity/teams_all.rs` reads the whole `Teams` table and refuses
    // to run when two rows share a display name, because `ORDER BY DisplayName` has no tiebreak —
    // and `cargo test --workspace` runs this file and that suite against the same database at the
    // same time. Two rows called "mmrs get many" took the entire `teams_all` module down once.
    for (id, delete_at, name, display) in [
        (LIVE, 0_i64, "mmrs-getmany-live", "mmrs get many live"),
        (
            ARCHIVED,
            1_788_636_490_000,
            "mmrs-getmany-archived",
            "mmrs get many archived",
        ),
    ] {
        sqlx::query(
            "INSERT INTO teams
                (id, createat, updateat, deleteat, displayname, name, description, email, type,
                 companyname, alloweddomains, inviteid, schemeid, allowopeninvite,
                 lastteamiconupdate, groupconstrained, cloudlimitsarchived)
             VALUES ($1, 1701355045000, 1701355045000, $2, $5, $3, '',
                     'mmrs-getmany@mmrs.invalid', 'O', '', '', $4, NULL, false, 0, NULL, false)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(name)
        .bind(format!("{id}invite"))
        .bind(display)
        .execute(pool)
        .await
        .expect("inserts the team");
    }
}

#[tokio::test]
async fn get_many_returns_archived_teams_and_calls_an_empty_result_not_found() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlTeamStore::new(pool.clone());

    let found = store
        .get_many(&[LIVE.to_owned(), ARCHIVED.to_owned()])
        .await
        .expect("both ids exist");
    let ids: std::collections::BTreeSet<&str> = found.iter().map(|team| team.id.as_str()).collect();
    assert_eq!(
        ids,
        [LIVE, ARCHIVED].into_iter().collect(),
        "there is no DeleteAt predicate, so the archived team comes back too"
    );
    assert!(
        found.iter().any(|team| team.delete_at != 0),
        "and it really is archived: {found:?}"
    );

    let err = store
        .get_many(&[ABSENT.to_owned()])
        .await
        .expect_err("no row matches");
    assert!(
        err.is_not_found(),
        "an empty result is a typed not-found, which the app layer answers 404 to: {err:?}"
    );

    let err = store
        .get_many(&[])
        .await
        .expect_err("an empty id list matches nothing");
    assert!(err.is_not_found(), "{err:?}");

    purge(&pool).await;
}
