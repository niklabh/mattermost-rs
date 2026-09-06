//! `SqlSessionStore`'s two writes — `update_last_activity_at` and `remove` — against a real
//! Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_session_activity
//! ```
//!
//! These are the first writes this store makes, and both are aimed at rows the **Go server also
//! owns**: `LastActivityAt` is what its idle-timeout check reads, and `Remove` is how that check
//! revokes. So the things worth pinning here are the ones a reader would get wrong in a way no
//! unit test could see — which column is written, which rows are matched, and which are left
//! alone. Every row here is `mmrs`-prefixed and purged either side; the fixture sessions belong
//! to no user and are never authenticated against.

use mm_store::{SessionStore, session_store::SqlSessionStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

/// The suite writes rows in a shared namespace, so its tests are serialised against each other.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SESSION_ID: &str = "mmrssessact00000000000sid1";
const SESSION_TOKEN: &str = "mmrssessact00000000000tok1";
const OTHER_ID: &str = "mmrssessact00000000000sid2";
const OTHER_TOKEN: &str = "mmrssessact00000000000tok2";

/// A recognisable "before" value: 2023-11-14T22:13:20Z in epoch millis.
const SEEDED_ACTIVITY: i64 = 1_700_000_000_000;

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        // Capped well under sqlx's 30-second default — see CLAUDE.md, "a test that waits is a bug".
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM sessions WHERE id LIKE 'mmrssessact%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// Two sessions, identical apart from their ids and tokens, so every write can be checked for
/// what it did **not** touch as well as what it did.
async fn seed(pool: &PgPool) {
    for (id, token) in [(SESSION_ID, SESSION_TOKEN), (OTHER_ID, OTHER_TOKEN)] {
        sqlx::query(
            "INSERT INTO sessions
                 (id, token, createat, expiresat, lastactivityat, userid, deviceid, roles,
                  isoauth, props, expirednotify, voipdeviceid)
             VALUES ($1, $2, $3, 0, $3, 'mmrssessact0000000000usr1', '', 'system_user',
                     false, '{}'::jsonb, false, '')",
        )
        .bind(id)
        .bind(token)
        .bind(SEEDED_ACTIVITY)
        .execute(pool)
        .await
        .expect("seeds a session row");
    }
}

async fn activity_of(pool: &PgPool, id: &str) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT lastactivityat FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("reads the column")
        .flatten()
}

async fn exists(pool: &PgPool, id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("counts")
        == 1
}

/// The write lands on `LastActivityAt`, on one row, and moves nothing else.
///
/// `Sessions` has five other bigint-ish columns and the neighbouring `UpdateExpiresAt`
/// (session_store.go:315) writes one of them from a signature of exactly the same shape, so
/// "which column" is a real thing to get wrong rather than a formality. `CreateAt` is seeded to
/// the same value as `LastActivityAt` on purpose: a write that hit the wrong column would
/// otherwise be invisible against a zero.
#[tokio::test]
async fn the_activity_write_moves_one_column_of_one_row() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlSessionStore::new(pool.clone());
    let now = SEEDED_ACTIVITY + 9_999;
    store
        .update_last_activity_at(SESSION_ID, now)
        .await
        .expect("the update runs");

    let row = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>)>(
        "SELECT lastactivityat, createat, expiresat FROM sessions WHERE id = $1",
    )
    .bind(SESSION_ID)
    .fetch_one(&pool)
    .await
    .expect("the row is still there");

    assert_eq!(row.0, Some(now), "LastActivityAt is written");
    assert_eq!(row.1, Some(SEEDED_ACTIVITY), "CreateAt is not");
    assert_eq!(row.2, Some(0), "ExpiresAt is not");
    assert_eq!(
        activity_of(&pool, OTHER_ID).await,
        Some(SEEDED_ACTIVITY),
        "and the other session is untouched — the WHERE is on Id, not on the user"
    );

    purge(&pool).await;
}

/// **The activity write matches `Id` only.** Go's `Get` and `Remove` both accept "id or token";
/// this one does not, and a port that made all three consistent would silently stop refreshing
/// sessions for any caller that passed a token — with no error, because an `UPDATE` matching
/// nothing succeeds.
#[tokio::test]
async fn the_activity_write_ignores_the_token_column() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlSessionStore::new(pool.clone());
    store
        .update_last_activity_at(SESSION_TOKEN, SEEDED_ACTIVITY + 9_999)
        .await
        .expect("a no-op update is not an error");

    assert_eq!(
        activity_of(&pool, SESSION_ID).await,
        Some(SEEDED_ACTIVITY),
        "passing the token updated nothing, and reported success — Go's behaviour exactly"
    );

    purge(&pool).await;
}

/// `Remove` takes an id **or** a token: one value against two columns.
#[tokio::test]
async fn remove_matches_either_the_id_or_the_token() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlSessionStore::new(pool.clone());

    store.remove(SESSION_ID).await.expect("removes by id");
    assert!(!exists(&pool, SESSION_ID).await, "the row is gone");
    assert!(
        exists(&pool, OTHER_ID).await,
        "and only that row — the second session is still there"
    );

    store.remove(OTHER_TOKEN).await.expect("removes by token");
    assert!(!exists(&pool, OTHER_ID).await, "the token side matches too");

    purge(&pool).await;
}

/// Removing a session that is not there is success, not a miss.
///
/// The one caller is the idle-timeout revoke, which runs on a request the client will be told to
/// re-authenticate for regardless. Returning `NotFound` here would turn a lost race between two
/// requests carrying the same expired token into a 500 for one of them.
#[tokio::test]
async fn removing_an_absent_session_is_not_an_error() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let store = SqlSessionStore::new(pool.clone());
    store
        .remove("mmrssessactnosuchsession01")
        .await
        .expect("deleting nothing succeeds");
}
