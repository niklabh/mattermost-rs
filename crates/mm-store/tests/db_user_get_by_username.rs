//! `SqlUserStore::get_by_username` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_get_by_username
//! ```
//!
//! What only this file can cover: the query lowers its **parameter** (`Username = lower(?)`,
//! user_store.go:1403), and that fold is unreachable over REST — `getUserByUsername`'s
//! `IsValidUsername` rejects anything uppercase before the store runs. The fold exists for Go's
//! login paths, which share the store method; a transcription pinned here, [D-151]'s shape.

use mm_store::{UserStore, user_store::SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER_ID: &str = "mmrsunameuser000000000usr1";
const USERNAME: &str = "mmrsuname-fixture.user_1";

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
    sqlx::query("DELETE FROM users WHERE id LIKE 'mmrsuname%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

#[tokio::test]
async fn the_parameter_is_folded_and_the_column_is_not() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, email, roles, lastlogin)
         VALUES ($1, 0, 0, 0, $2, $1 || '@mmrs.invalid', 'system_user', 0)",
    )
    .bind(USER_ID)
    .bind(USERNAME)
    .execute(&pool)
    .await
    .expect("inserts the user");

    let store = SqlUserStore::new(pool.clone());

    // Exact lowercase finds the row; an uppercase *parameter* is folded and finds it too —
    // Go's `lower(?)` applies to the input, never the column.
    let exact = store.get_by_username(USERNAME).await.expect("found");
    assert_eq!(exact.id, USER_ID);
    let folded = store
        .get_by_username(&USERNAME.to_uppercase())
        .await
        .expect("the parameter is lowered before comparing");
    assert_eq!(folded.id, USER_ID);
    assert_eq!(
        folded.username, USERNAME,
        "the stored value comes back as stored, not as queried"
    );

    let missing = store
        .get_by_username("mmrsuname-no-such-user")
        .await
        .expect_err("no row");
    assert!(matches!(
        missing,
        mm_store::StoreError::NotFound { entity: "User", .. }
    ));

    purge(&pool).await;
}
