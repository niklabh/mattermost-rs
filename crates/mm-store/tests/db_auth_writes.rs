//! The authentication write primitives against a real Postgres.
//!
//! ```sh
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_auth_writes
//! ```
//!
//! **What only this file can cover.** Three of these five statements encode a decision that no
//! HTTP response reveals:
//!
//! - `UpdatePassword` writes **six** columns, three of which the caller never asked about —
//!   `AuthData = NULL`, `AuthService = ''`, `FailedAttempts = 0`. A password change is therefore
//!   also an account conversion and a lockout reset. Every route that reaches it answers
//!   `{"status":"OK"}` either way, so a port that wrote only `Password` looks identical on the
//!   wire and leaves a SAML user with a password they cannot use and a locked account still
//!   locked.
//! - `TryIncrementFailedPasswordAttempts`'s predicate is strictly `<`, and the *boundary* is the
//!   whole behaviour: off by one in either direction changes how many attempts an account gets
//!   before it locks. From the API that is only visible after N requests in a row, which is a
//!   test the parity suite cannot afford to run against two servers.
//! - `DecrementFailedPasswordAttempts` floors at zero with `AND FailedAttempts > 0` rather than
//!   with arithmetic. A refund with nothing to refund must leave `0`, not `-1` — and a `-1` is
//!   invisible until it silently grants an extra attempt.
//!
//! The token round trip is here rather than in the parity suite for a different reason: `Save` has
//! no route behind it (the minting routes stay with Go, [D-235]), so this is its only caller.

use mm_model::token::{TOKEN_TYPE_PASSWORD_RECOVERY, Token};
use mm_store::{TokenStore, UserStore, token_store::SqlTokenStore, user_store::SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Serialises the fixture rows: every test in this file plants under the same id prefix and the
/// purge is global to it.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER_ID: &str = "mmrsauthwrite00000000usr01";

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
    sqlx::query("DELETE FROM users WHERE id LIKE 'mmrsauthwrite%'")
        .execute(pool)
        .await
        .expect("purges leftover user rows");
    sqlx::query("DELETE FROM tokens WHERE extra LIKE '%mmrsauthwrite%'")
        .execute(pool)
        .await
        .expect("purges leftover token rows");
}

/// A user with a distinctive non-zero value in every column `UpdatePassword` touches, so a
/// dropped `SET` shows up as the planted value rather than coinciding with a zero.
///
/// `authservice`/`authdata` are populated *because* the interesting assertion is that they are
/// cleared; `failedattempts` starts at 4 for the same reason.
async fn plant_user(pool: &PgPool, failed_attempts: i32) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                            authservice, email, emailverified, roles, allowmarketing, props,
                            notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts,
                            locale, timezone, mfaactive, mfasecret, mfausedtimestamps, remoteid,
                            lastlogin)
         VALUES ($1, 1700000000001, 1700000000002, 0, 'mmrsauthwriteuser', 'planted-hash',
                 'mmrsauthwrite-authdata', 'saml', 'MMRSAuthWrite@Example.COM', FALSE,
                 'system_user', TRUE, '{}'::jsonb, '{}'::jsonb, 1700000000003, 1700000000004,
                 $2, 'en', '{}'::jsonb, FALSE, '', '[]'::jsonb, '', 0)",
    )
    .bind(USER_ID)
    .bind(failed_attempts)
    .execute(pool)
    .await
    .expect("plants the fixture user");
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    password: Option<String>,
    authdata: Option<String>,
    authservice: Option<String>,
    failedattempts: Option<i32>,
    lastpasswordupdate: Option<i64>,
    updateat: Option<i64>,
    email: Option<String>,
    emailverified: Option<bool>,
}

async fn read(pool: &PgPool) -> Row {
    sqlx::query_as::<_, Row>(
        "SELECT password, authdata, authservice, failedattempts, lastpasswordupdate, updateat,
                email, emailverified
           FROM users WHERE id = $1",
    )
    .bind(USER_ID)
    .fetch_one(pool)
    .await
    .expect("reads the fixture user back")
}

/// The tests live in a module so that `cargo test -p mm-store --tests auth_writes` selects them.
///
/// libtest filters on the **test's own name**, which for an integration test is the bare function
/// name — no file, no crate. A filter naming the file matches nothing, runs zero tests, exits 0,
/// and a mutation harness reads that as SURVIVED. Three store mutations were reported that way
/// before this module existed; the module is what makes the filter in
/// `scripts/mutations/auth-writes.plan` select anything at all.
mod auth_writes {
    use super::*;

    /// The six columns, one assertion each — including the three a caller never named.
    #[tokio::test]
    async fn update_password_clears_the_auth_fields_and_the_lockout() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;
        plant_user(&pool, 4).await;

        let store = SqlUserStore::new(pool.clone());
        store
            .update_password(USER_ID, "the-new-hash")
            .await
            .expect("updates");

        let row = read(&pool).await;
        assert_eq!(row.password.as_deref(), Some("the-new-hash"));
        assert_eq!(
            row.authdata, None,
            "AuthData is set to SQL NULL, not to the empty string"
        );
        assert_eq!(
            row.authservice.as_deref(),
            Some(""),
            "AuthService is set to the empty string, not to NULL — the two are different columns \
             with different spellings of 'none'"
        );
        assert_eq!(
            row.failedattempts,
            Some(0),
            "a password change also unlocks the account"
        );
        assert_ne!(row.lastpasswordupdate, Some(1_700_000_000_003));
        assert_ne!(row.updateat, Some(1_700_000_000_002));
        assert_eq!(
            row.lastpasswordupdate, row.updateat,
            "Go reads GetMillis() once into `updateAt` and binds it to both columns"
        );

        purge(&pool).await;
    }

    /// The cap is the *number of attempts allowed*: with a max of 3 and a counter at 0, exactly three
    /// claims succeed and the fourth does not. The boundary is the assertion — `<=` would give four.
    #[tokio::test]
    async fn the_claim_stops_exactly_at_the_cap() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;
        plant_user(&pool, 0).await;

        let store = SqlUserStore::new(pool.clone());
        for attempt in 1..=3 {
            assert!(
                store
                    .try_increment_failed_password_attempts(USER_ID, 3)
                    .await
                    .expect("claims"),
                "attempt {attempt} of 3 must be allowed"
            );
        }
        assert!(
            !store
                .try_increment_failed_password_attempts(USER_ID, 3)
                .await
                .expect("claims"),
            "the fourth attempt against a cap of 3 must be refused"
        );
        assert_eq!(
            read(&pool).await.failedattempts,
            Some(3),
            "a refused claim must not increment"
        );

        purge(&pool).await;
    }

    /// A nonexistent user is `false`, not an error — which is what lets the lockout refusal be the
    /// same answer for an unknown account as for a locked one.
    #[tokio::test]
    async fn claiming_against_a_missing_user_is_false_rather_than_an_error() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;

        let store = SqlUserStore::new(pool.clone());
        assert!(
            !store
                .try_increment_failed_password_attempts("mmrsauthwrite00000000nobod", 10)
                .await
                .expect("no error")
        );
    }

    /// The refund floors at zero, and a refund with nothing to refund is success.
    #[tokio::test]
    async fn the_refund_floors_at_zero() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;
        plant_user(&pool, 1).await;

        let store = SqlUserStore::new(pool.clone());
        store
            .decrement_failed_password_attempts(USER_ID)
            .await
            .expect("refunds");
        assert_eq!(read(&pool).await.failedattempts, Some(0));

        store
            .decrement_failed_password_attempts(USER_ID)
            .await
            .expect("a refund with nothing to refund is success");
        assert_eq!(
            read(&pool).await.failedattempts,
            Some(0),
            "the floor is the WHERE clause, not arithmetic — never -1"
        );

        purge(&pool).await;
    }

    /// `UpdateFailedPasswordAttempts` is an unconditional set, and it does **not** touch `UpdateAt`.
    #[tokio::test]
    async fn setting_the_counter_leaves_update_at_alone() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;
        plant_user(&pool, 9).await;

        let store = SqlUserStore::new(pool.clone());
        store
            .update_failed_password_attempts(USER_ID, 0)
            .await
            .expect("sets");

        let row = read(&pool).await;
        assert_eq!(row.failedattempts, Some(0));
        assert_eq!(
            row.updateat,
            Some(1_700_000_000_002),
            "clearing the counter is not an update to the user"
        );

        purge(&pool).await;
    }

    /// The address is lower-cased **in SQL**, so a token minted with a mixed-case address still lands
    /// as lower case — and the flag and `UpdateAt` move with it.
    #[tokio::test]
    async fn verify_email_lowercases_in_sql() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;
        plant_user(&pool, 0).await;

        let store = SqlUserStore::new(pool.clone());
        store
            .verify_email(USER_ID, "MMRSAuthWrite+Verified@Example.COM")
            .await
            .expect("verifies");

        let row = read(&pool).await;
        assert_eq!(
            row.email.as_deref(),
            Some("mmrsauthwrite+verified@example.com")
        );
        assert_eq!(row.emailverified, Some(true));
        assert_ne!(row.updateat, Some(1_700_000_000_002));

        purge(&pool).await;
    }

    /// Save, read back field for field, delete, and miss. `Save`'s only caller.
    #[tokio::test]
    async fn a_token_round_trips_and_deletes() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        purge(&pool).await;

        let store = SqlTokenStore::new(pool.clone());
        let token = Token {
            token: "m".repeat(64),
            create_at: 1_700_000_000_123,
            type_: TOKEN_TYPE_PASSWORD_RECOVERY.to_owned(),
            // The Go-cased shape a real token carries, confirmed against a row Go minted.
            extra: r#"{"UserId":"mmrsauthwrite00000000usr01","Email":"mmrsauthwrite@example.com"}"#
                .to_owned(),
        };
        store.save(&token).await.expect("saves");

        let read_back = store.get_by_token(&token.token).await.expect("reads back");
        assert_eq!(read_back, token);

        store.delete(&token.token).await.expect("deletes");
        let err = store
            .get_by_token(&token.token)
            .await
            .expect_err("the row is gone");
        assert!(err.is_not_found(), "a miss is NotFound, not a driver error");
        assert!(
            !err.to_string().contains(&token.token),
            "the miss message must not carry the token itself"
        );

        store.delete(&token.token).await.expect(
            "deleting an absent token is success, which is what makes consume-on-use idempotent",
        );

        purge(&pool).await;
    }

    /// `IsValid` runs **in the store**, before the insert, and returns the model's own `AppError` —
    /// a 500 with `model.token.is_valid.size`, not a driver error about a `varchar(64)`.
    #[tokio::test]
    async fn a_malformed_token_is_refused_before_the_insert() {
        if !db_enabled() {
            eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
            return;
        }
        let _guard = FIXTURES.lock().await;
        let pool = pool().await;
        let store = SqlTokenStore::new(pool.clone());

        let short = Token {
            token: "too-short".to_owned(),
            create_at: 1,
            type_: TOKEN_TYPE_PASSWORD_RECOVERY.to_owned(),
            extra: String::new(),
        };
        let err = store.save(&short).await.expect_err("refused");
        assert!(err.to_string().contains("model.token.is_valid.size"));

        let zero_create_at = Token {
            token: "z".repeat(64),
            create_at: 0,
            type_: TOKEN_TYPE_PASSWORD_RECOVERY.to_owned(),
            extra: String::new(),
        };
        let err = store.save(&zero_create_at).await.expect_err("refused");
        assert!(
            err.to_string().contains("model.token.is_valid.expiry"),
            "the id says 'expiry' while the branch tests CreateAt — Go's wording, kept"
        );
    }
}
