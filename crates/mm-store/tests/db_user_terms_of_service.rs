//! `SqlUserTermsOfServiceStore::get_by_user` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_terms_of_service
//! ```
//!
//! The parity suite also drives this store over REST (with a directly-inserted row, since Team
//! Edition cannot author a terms of service); what only this file covers is the NULL columns —
//! `termsofserviceid` and `createat` are nullable and reachable only by direct insert.

use mm_store::user_terms_of_service_store::get_by_user;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsutosuser0000000000full";
const USER_NULLS: &str = "mmrsutosuser0000000000null";
const TOS_ID: &str = "mmrsutostos000000000000001";

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
    sqlx::query("DELETE FROM usertermsofservice WHERE userid LIKE 'mmrsutos%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

#[tokio::test]
async fn a_row_reads_back_and_null_columns_default() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    sqlx::query(
        "INSERT INTO usertermsofservice (userid, termsofserviceid, createat)
         VALUES ($1, $2, 1700000000000), ($3, NULL, NULL)",
    )
    .bind(USER)
    .bind(TOS_ID)
    .bind(USER_NULLS)
    .execute(&pool)
    .await
    .expect("inserts the fixture rows");

    let full = get_by_user(&pool, USER).await.expect("the row reads back");
    assert_eq!(full.user_id, USER);
    assert_eq!(full.terms_of_service_id, TOS_ID);
    assert_eq!(full.create_at, 1_700_000_000_000);

    let nulls = get_by_user(&pool, USER_NULLS).await.expect("reads back");
    assert_eq!(
        nulls.terms_of_service_id, "",
        "SQL NULL scans as the zero value, as in Go"
    );
    assert_eq!(nulls.create_at, 0);

    let missing = get_by_user(&pool, "zzzzzzzzzzzzzzzzzzzzzzzzzz")
        .await
        .expect_err("no row");
    assert!(
        matches!(
            missing,
            mm_store::StoreError::NotFound {
                entity: "UserTermsOfService",
                ..
            }
        ),
        "Go's ErrNotFound names the entity"
    );

    purge(&pool).await;
}
