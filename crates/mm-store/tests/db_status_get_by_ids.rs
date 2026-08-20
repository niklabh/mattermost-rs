//! `SqlStatusStore::get_by_ids` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_status_get_by_ids
//! ```
//!
//! Two things only a database can show: that the `= ANY` predicate returns exactly the asked-for
//! rows and nothing adjacent, and that every `COALESCE` in Go's select does its job — each column
//! but the key is nullable, and nothing reachable over REST writes a NULL, so the row with NULLs
//! is planted here directly.

use mm_store::{SqlStatusStore, StatusStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const FULL: &str = "mmrsstatus000000000000full";
const NULLS: &str = "mmrsstatus00000000000nulls";
const BYSTANDER: &str = "mmrsstatus0000000bystander";
const ABSENT: &str = "mmrsstatus00000000000absnt";

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
    sqlx::query("DELETE FROM status WHERE userid LIKE 'mmrsstatus%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

async fn seed(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO status (userid, status, manual, lastactivityat, dndendtime, prevstatus)
         VALUES ($1, 'dnd', true, 1701355039000, 1701358639, 'online')",
    )
    .bind(FULL)
    .execute(pool)
    .await
    .expect("inserts the populated row");

    sqlx::query("INSERT INTO status (userid) VALUES ($1)")
        .bind(NULLS)
        .execute(pool)
        .await
        .expect("inserts the all-NULL row");

    sqlx::query(
        "INSERT INTO status (userid, status, manual, lastactivityat, dndendtime, prevstatus)
         VALUES ($1, 'away', false, 1701355039001, 0, '')",
    )
    .bind(BYSTANDER)
    .execute(pool)
    .await
    .expect("inserts the bystander");
}

#[tokio::test]
async fn returns_exactly_the_asked_for_rows_with_nulls_coalesced() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlStatusStore::new(pool.clone());
    let mut statuses = store
        .get_by_ids(&[FULL.to_owned(), NULLS.to_owned(), ABSENT.to_owned()])
        .await
        .expect("queries");
    // No ORDER BY in Go or here; sort for the assertion only.
    statuses.sort_by(|a, b| a.user_id.cmp(&b.user_id));

    assert_eq!(
        statuses.len(),
        2,
        "the absent id yields no row (the app layer synthesises it) and the bystander is not asked for"
    );

    let full = &statuses[0];
    assert_eq!(full.user_id, FULL);
    assert_eq!(full.status, "dnd");
    assert!(full.manual);
    assert_eq!(full.last_activity_at, 1_701_355_039_000);
    assert_eq!(full.dnd_end_time, 1_701_358_639);
    assert_eq!(full.prev_status, "online");
    assert_eq!(full.active_channel, "", "no column, never populated");

    let nulls = &statuses[1];
    assert_eq!(nulls.user_id, NULLS);
    assert_eq!(nulls.status, "", "COALESCE(Status, '')");
    assert!(!nulls.manual, "COALESCE(Manual, FALSE)");
    assert_eq!(nulls.last_activity_at, 0, "COALESCE(LastActivityAt, 0)");
    assert_eq!(nulls.dnd_end_time, 0, "COALESCE(DNDEndTime, 0)");
    assert_eq!(nulls.prev_status, "", "COALESCE(PrevStatus, '')");

    purge(&pool).await;
}

#[tokio::test]
async fn an_empty_ask_and_an_all_absent_ask_are_both_empty() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlStatusStore::new(pool.clone());
    assert!(store.get_by_ids(&[]).await.expect("queries").is_empty());
    assert!(
        store
            .get_by_ids(&[ABSENT.to_owned()])
            .await
            .expect("queries")
            .is_empty(),
        "a miss is an empty list, not an error — the 404 branch upstream is not this"
    );

    purge(&pool).await;
}
