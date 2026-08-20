//! `App::get_user_terms_of_service`'s 404 mapping against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_user_terms_of_service
//! ```
//!
//! The unit suite can only reach the 500 branch (an unreachable store cannot produce a clean
//! miss), and over HTTP the 404 is swallowed by `getUser`'s ignore-a-miss guard — so without
//! this test a mutation of the `no_rows.` id would survive everything.

use mm_app::App;
use mm_store::SqlStore;
use sqlx::postgres::PgPoolOptions;

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

#[tokio::test]
async fn a_clean_miss_is_a_404_with_the_no_rows_id() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connects to Postgres");
    let app = App::new(SqlStore::from_pool(pool));

    let err = app
        .get_user_terms_of_service("zzzzzzzzzzzzzzzzzzzzzzzzzz")
        .await
        .expect_err("no such row");
    assert_eq!(err.status_code, 404);
    assert_eq!(
        err.id, "app.user_terms_of_service.get_by_user.no_rows.app_error",
        "the miss id inserts no_rows. into the failure id (user_terms_of_service.go:20)"
    );
    assert_eq!(err.where_, "GetUserTermsOfService");
}
