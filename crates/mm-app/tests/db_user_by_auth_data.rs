//! `App::get_user_by_auth_data`'s three status codes.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_user_by_auth_data
//! ```
//!
//! # Why this is not a parity test
//!
//! Go maps three store errors onto **one error id** and three different statuses: 400 for
//! `ErrInvalidInput`, 404 for `ErrNotFound`, 500 for anything else. The 400 is unreachable
//! through `GET /api/v4/users/auth_data`, because the handler rejects an empty `value` with its
//! own `SetInvalidParam` two lines before the app is called — so no cross-server test can tell
//! the 400 branch from the 404 one, and a mutation swapping them survives the whole parity suite.
//! Measured: `app-authdata-invalid-input-is-a-404` in
//! `scripts/mutations/user-lookups.plan` did exactly that.
//!
//! The branch is ported anyway, because it is a status code a later caller would meet, and it is
//! tested here, where the app can be called directly.

use mm_app::App;
use mm_store::SqlStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One mutable database, shared with the `mm-store` suite.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER_ID: &str = "mmrsauthdataapp00000000001";
const AUTH_DATA: &str = "mmrs-app-auth-data";

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
    sqlx::query("DELETE FROM users WHERE id LIKE 'mmrsauthdataapp%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// `MfaUsedTimestamps` is `'null'::jsonb`, not `'{}'`: Go scans it into a `model.StringArray` and
/// an object there makes **Go** answer 500 to any `GET /api/v4/users` returning the row.
async fn seed(pool: &PgPool) {
    purge(pool).await;
    sqlx::query(
        "INSERT INTO users
            (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
             emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
             notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
             mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
         VALUES ($1, 1788600000000, 1788600000000, 0, 'mmrsauthdataapp', '', $2, '',
                 'mmrsauthdataapp@mmrs.invalid', false, '', '', '', '', 'system_user', false,
                 '{}'::jsonb, '{}'::jsonb, 1788600000000, 0, 0, 'en', '{}'::jsonb, false, '',
                 NULL, 0, 'null'::jsonb)",
    )
    .bind(USER_ID)
    .bind(AUTH_DATA)
    .execute(pool)
    .await
    .expect("the user row is written");
}

/// A hit, a miss and an empty key — three statuses under one id.
#[tokio::test]
async fn app_auth_data_maps_three_store_errors_onto_three_statuses() {
    if !db_enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    let found = app
        .get_user_by_auth_data(AUTH_DATA)
        .await
        .expect("the planted auth data resolves");
    assert_eq!(found.id, USER_ID);

    let missing = app
        .get_user_by_auth_data("mmrs-no-such-auth-data")
        .await
        .expect_err("a miss is an error");
    assert_eq!(missing.status_code, 404, "ErrNotFound is a 404");
    assert_eq!(missing.id, "app.user.missing_account.const");

    // **The unreachable one.** `getUserByAuthData` refuses an empty `value` before the app is
    // called, so this 400 has no route above it — and the id is the *same* as the 404's, which
    // is what makes the status the only thing distinguishing them.
    let empty = app
        .get_user_by_auth_data("")
        .await
        .expect_err("an empty key is an error");
    assert_eq!(
        empty.status_code, 400,
        "ErrInvalidInput is a 400, not a 404"
    );
    assert_eq!(
        empty.id, "app.user.missing_account.const",
        "one id for all three outcomes — a client can only tell them apart by status"
    );
    assert_eq!(empty.where_, "GetUserByAuthData");

    purge(&pool).await;
}
