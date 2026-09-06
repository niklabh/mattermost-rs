//! `SqlOAuthStore::get_app_by_user` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_oauth_apps_by_creator
//! ```
//!
//! # Why this is not in the parity suite
//!
//! `getOAuthApps` picks `GetAppByUser` only for a caller who has `manage_oauth` and **not**
//! `manage_system_wide_oauth` — and on a stock server the single role granting either,
//! `system_admin`, grants both. So the branch cannot be reached over HTTP, and a mutation of the
//! creator predicate survives the whole parity suite. Same shape as the webhook owner filter.
//!
//! Note also that the filter here is **unconditional**, unlike the webhook stores' — Go adds it
//! with a plain `Where` (oauth_store.go:126) — so an empty user id matches nothing rather than
//! everything. That difference is the point of the last assertion.

use mm_store::{OAuthStore, SqlOAuthStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MINE: &str = "mmrsoauthby0000000000mine1";
const THEIRS: &str = "mmrsoauthby000000000their1";
const OWNER: &str = "mmrsoauthby0000000000owner";
const OTHER: &str = "mmrsoauthby0000000000other";

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
    sqlx::query("DELETE FROM oauthapps WHERE id LIKE 'mmrsoauthby%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

async fn seed(pool: &PgPool) {
    for (id, creator, name) in [
        (MINE, OWNER, "mmrs by creator mine"),
        (THEIRS, OTHER, "mmrs by creator theirs"),
    ] {
        sqlx::query(
            "INSERT INTO oauthapps
                (id, creatorid, createat, updateat, clientsecret, name, description,
                 callbackurls, homepage, istrusted, iconurl, mattermostappid,
                 isdynamicallyregistered)
             VALUES ($1, $2, 1788636490668, 1788636490669, 'mmrssecret000000000000001',
                     $3, 'planted by the store test', '[\"http://example.invalid/a\"]',
                     'http://example.invalid/', false, '', '', false)",
        )
        .bind(id)
        .bind(creator)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the oauth app");
    }
}

#[tokio::test]
async fn by_creator_narrows_to_one_owner_and_an_empty_id_matches_nothing() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlOAuthStore::new(pool.clone());

    let mine = store
        .get_app_by_user(OWNER, 0, 60)
        .await
        .expect("the query runs");
    assert_eq!(
        mine.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
        vec![MINE],
        "a named creator gets only their own apps"
    );

    let theirs = store
        .get_app_by_user(OTHER, 0, 60)
        .await
        .expect("the query runs");
    assert_eq!(
        theirs.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
        vec![THEIRS]
    );

    // **Unconditional predicate.** The webhook stores treat an empty owner as "every owner"
    // because Go guards those filters with `if userId != ""`; this one has no guard, so an empty
    // id is a creator nobody has.
    let none = store
        .get_app_by_user("", 0, 60)
        .await
        .expect("the query runs");
    assert!(
        none.is_empty(),
        "an empty creator id matches nothing, not everything: {none:?}"
    );

    // And the unfiltered list does see both, so the assertions above are about the predicate.
    let all = store.get_apps(0, 1000).await.expect("the query runs");
    let ids: std::collections::BTreeSet<&str> = all.iter().map(|app| app.id.as_str()).collect();
    assert!(ids.contains(MINE) && ids.contains(THEIRS), "{ids:?}");

    purge(&pool).await;
}
