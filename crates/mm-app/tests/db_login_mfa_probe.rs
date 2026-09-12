//! `App::login_needs_mfa` against a real Postgres — the probe that decides whether `login` hands
//! the request to Go, and the claim that it **writes nothing**.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_login_mfa_probe
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! The development stack has `ServiceSettings.EnableMultifactorAuthentication` off and no enrolled
//! account, so *both* servers skip MFA entirely and the branch is invisible over HTTP. Turning the
//! setting on would change the Go server's configuration for every other suite in the binary, and
//! the row it needs (`Users.MfaActive = true`) cannot be created through the API without a real
//! TOTP enrolment.
//!
//! So the property that matters is asserted here instead, where the function returns its own
//! value and the column can be read directly:
//!
//! **`login_needs_mfa` decides before anything is written.** Go's `CheckUserMfa` runs *after*
//! `TryIncrementFailedPasswordAttempts` has claimed a slot (authentication.go:127-153). A port
//! that discovered it needed to forward at that point would leave the counter moved and let Go
//! move it again for the same attempt — one password guess, two failures recorded. `mm_api::login`
//! therefore asks the question one `SELECT` earlier, and what makes that safe is that the question
//! itself does not write. That is what the `_writes_nothing` tests assert, on both arms: the one
//! that forwards and the one that does not.
//!
//! Every test is named `login_needs_mfa_*` so `MUTATE_FILTER` can select them by name.

use mm_app::App;
use mm_app::config::Config;
use mm_store::SqlStore;
use sqlx::postgres::PgPoolOptions;

/// One id and username **per test**, because `cargo test` runs the three below on threads of one
/// binary and a shared fixture row makes them race: the first run of this file had both planting
/// `mfaprobeuser00000000000001` and the loser failing on the primary key.
fn ids(tag: &str) -> (String, String, String) {
    (
        format!("mfaprobeuser000000000000{tag}"),
        format!("mmrsmfaprobe{tag}"),
        format!("mmrsmfaprobe{tag}@mmrs.invalid"),
    )
}

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

fn app_with(pool: sqlx::PgPool, mfa_enabled: bool) -> App {
    App::with_config(
        SqlStore::from_pool(pool),
        Config {
            enable_multifactor_authentication: mfa_enabled,
            enable_sign_in_with_email: true,
            enable_sign_in_with_username: true,
            ..Config::default()
        },
    )
}

/// Plant the row the probe has to find, with `MfaActive` as given and the counter at a **non-zero**
/// value.
///
/// Non-zero on purpose: a counter that starts at `0` is `0` afterwards whether the probe wrote to
/// it or not, so the "writes nothing" assertion would hold against a probe that zeroed the column.
/// Seven is arbitrary and distinctive.
async fn plant(pool: &sqlx::PgPool, user_id: &str, username: &str, email: &str, mfa_active: bool) {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("the old row clears");
    sqlx::query(
        "INSERT INTO users
           (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
            emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
            notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
            mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
         VALUES ($1, 1788600000000, 1788600000000, 0, $2, '', NULL, '', $3,
                 false, '', 'MFA Probe', '', '', 'system_user', false,
                 '{}'::jsonb, '{}'::jsonb, 1788600000000, 0, 7, 'en', '{}'::jsonb,
                 $4, '', NULL, 0, 'null'::jsonb)",
    )
    .bind(user_id)
    .bind(username)
    .bind(email)
    .bind(mfa_active)
    .execute(pool)
    .await
    .expect("the fixture row is planted");
}

async fn failed_attempts(pool: &sqlx::PgPool, user_id: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT failedattempts FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the row is readable")
}

async fn clean(pool: &sqlx::PgPool, user_id: &str) {
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

/// An enrolled account on an MFA-enabled server: the probe says **forward**, and the counter is
/// exactly where it was.
///
/// Both halves are the test. The first says the forward happens at all; the second says it happens
/// before any write, which is the whole reason the probe exists this early in the handler.
#[tokio::test]
async fn login_needs_mfa_forwards_an_enrolled_account_and_writes_nothing() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    let (user_id, username, email) = ids("01");
    plant(&pool, &user_id, &username, &email, true).await;

    let app = app_with(pool.clone(), true);
    assert!(
        app.login_needs_mfa("", &username).await,
        "an enrolled account on an MFA-enabled server must be handed to Go"
    );
    assert_eq!(
        failed_attempts(&pool, &user_id).await,
        7,
        "the probe must not touch the failed-attempt counter"
    );

    // And by the explicit id, which takes `GetUserForLogin`'s other branch entirely.
    assert!(app.login_needs_mfa(&user_id, "").await);
    assert_eq!(failed_attempts(&pool, &user_id).await, 7);

    clean(&pool, &user_id).await;
}

/// The two ways the probe says **serve**, each with the counter untouched.
///
/// Without these the test above would pass against a probe that returned `true` unconditionally —
/// which would forward every login on the stack and make the whole route unported without any
/// assertion noticing.
#[tokio::test]
async fn login_needs_mfa_serves_both_negative_arms_and_writes_nothing() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    let (user_id, username, email) = ids("02");

    // Enrolled, but the server has MFA off — `CheckUserMfa`'s first conjunct.
    plant(&pool, &user_id, &username, &email, true).await;
    assert!(
        !app_with(pool.clone(), false)
            .login_needs_mfa("", &username)
            .await,
        "MFA off on the server means nothing to forward"
    );
    assert_eq!(failed_attempts(&pool, &user_id).await, 7);

    // Server has MFA on, account is not enrolled — the second conjunct.
    plant(&pool, &user_id, &username, &email, false).await;
    assert!(
        !app_with(pool.clone(), true)
            .login_needs_mfa("", &username)
            .await,
        "an account with no second factor takes the local path"
    );
    assert_eq!(failed_attempts(&pool, &user_id).await, 7);

    clean(&pool, &user_id).await;
}

/// A login id that resolves to nothing answers `false` rather than forwarding — and still writes
/// nothing, because there is no row to write to.
///
/// The answer matters: forwarding an unknown account would hand Go every mistyped e-mail address
/// on an MFA-enabled server, and the refusal that follows is identical on both servers anyway.
#[tokio::test]
async fn login_needs_mfa_serves_an_unknown_account() {
    if !enabled() {
        return;
    }
    let pool = pool().await;
    let app = app_with(pool.clone(), true);

    assert!(!app.login_needs_mfa("", "mmrs-nobody-at-all").await);
    // A syntactically valid id that names nobody takes the other branch.
    assert!(!app.login_needs_mfa("zzzzzzzzzzzzzzzzzzzzzzzzzz", "").await);
}
