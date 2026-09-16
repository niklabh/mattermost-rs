//! `App::create_user` marks the cached product notices as viewed, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_new_user_notices
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! The ids come from the process's notice cache, which is filled from `NoticesURL`. The stack
//! servers cannot reach a feed, so over HTTP both caches are empty, both servers write nothing,
//! and dropping the call entirely would pass. Here the cache is planted.
//!
//! Every test is named `new_user_notices_*` so `MUTATE_FILTER` can select them by name.

use mm_app::App;
use mm_model::product_notices::{ProductNotice, ProductNotices};
use mm_model::user::User;
use mm_store::SqlStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const USERNAME: &str = "mmrsnvnuser";
const NOTICES: [&str; 2] = ["mmrsnvn_first", "mmrsnvn_second"];

fn enabled() -> bool {
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

/// Purges at the **start**, because a failing assertion panics past any trailing cleanup.
async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM productnoticeviewstate WHERE noticeid LIKE 'mmrsnvn%'",
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = 'mmrsnvnuser')",
        "DELETE FROM users WHERE username = 'mmrsnvnuser'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover fixtures");
    }
}

fn new_user() -> User {
    User {
        username: USERNAME.to_owned(),
        email: format!("{USERNAME}@mmrs.invalid"),
        password: "Mmrs-Nvn-12345".to_owned(),
        ..User::default()
    }
}

#[tokio::test]
async fn new_user_notices_are_marked_viewed_once() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    purge(&pool).await;

    let app = App::new(SqlStore::from_pool(pool.clone()));
    app.notices_cache()
        .write()
        .expect("an unpoisoned cache")
        .notices = ProductNotices(
        NOTICES
            .iter()
            .map(|id| ProductNotice {
                id: (*id).to_owned(),
                ..ProductNotice::default()
            })
            .collect(),
    );

    let created = app
        .create_user(&new_user())
        .await
        .expect("the user is created");

    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT noticeid, viewed FROM productnoticeviewstate WHERE userid = $1 ORDER BY noticeid",
    )
    .bind(&created.id)
    .fetch_all(&pool)
    .await
    .expect("reads the view state");
    purge(&pool).await;

    assert_eq!(
        rows,
        vec![(NOTICES[0].to_owned(), 1), (NOTICES[1].to_owned(), 1)],
        "every cached notice, viewed exactly once"
    );
}
