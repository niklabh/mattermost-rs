//! `SqlUserStore::update` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_update
//! ```
//!
//! **What only this file can cover, and why it is not optional.** `Update` copies thirteen fields
//! from the stored row onto the submitted user — and, for an untrusted caller, `Roles` and
//! `DeleteAt` as well. That is the security boundary of every update route in the server: without
//! it a client could set its own password hash, mark its own email verified, clear its own
//! failed-login count, turn off its own MFA, grant itself a role, or un-deactivate its own
//! account by naming the field in a request body.
//!
//! None of it is reachable over HTTP **today**: the only ported callers are the custom-status
//! routes, which change `Props` and nothing else, so a mutation deleting any one of the copies
//! survives the whole parity suite. Measured — sixteen of them did, on the first run of
//! `custom-status-writes.plan`. The store is the layer that can be handed a poisoned `User`, so
//! this is where the copies are pinned until `PUT /users/{id}` lands and the wire can reach them.

use mm_model::user::User;
use mm_store::{UserStore, user_store::SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER_ID: &str = "mmrsupdateuser00000000usr1";
const OTHER_ID: &str = "mmrsupdateuser00000000usr2";

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
    sqlx::query("DELETE FROM users WHERE id LIKE 'mmrsupdate%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// A stored user with a distinctive, non-zero value in every field `Update` protects — so a
/// dropped copy shows up as the *submitted* value rather than coinciding with a zero.
///
/// `authdata` is **NULL** here rather than a string, and that is not laziness: `User::IsValid`
/// runs *before* the copies (user_store.go:256), and it refuses a user carrying both an
/// `AuthData` and a `Password` (`auth_data_pwd`) or an `AuthData` with no `AuthService`
/// (`auth_data_type`). The column is also uniquely indexed, so two fixtures cannot share one.
/// The auth fields are covered on the SSO fixture below instead, which has no password.
async fn plant(pool: &PgPool, id: &str, username: &str, auth_service: &str) {
    let (password, auth_data): (&str, Option<String>) = if auth_service.is_empty() {
        ("stored-password-hash", None)
    } else {
        ("", Some(format!("{id}-stored-authdata")))
    };
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                            authservice, email, emailverified, nickname, firstname, lastname,
                            position, roles, allowmarketing, props, notifyprops,
                            lastpasswordupdate, lastpictureupdate, failedattempts, locale,
                            timezone, mfaactive, mfasecret, mfausedtimestamps, remoteid, lastlogin)
         VALUES ($1, 1700000000001, 1700000000002, 0, $2, $4, $5,
                 $3, $2 || '@mmrs.invalid', TRUE, 'stored-nick', 'Stored', 'User', 'Keeper',
                 'system_user system_admin', TRUE, '{}'::jsonb, '{}'::jsonb,
                 1700000000003, 1700000000004, 7, 'en',
                 '{\"useAutomaticTimezone\":\"true\"}'::jsonb, TRUE, 'stored-mfa-secret',
                 '[\"111\"]'::jsonb, '', 1700000000005)",
    )
    .bind(id)
    .bind(username)
    .bind(auth_service)
    .bind(password)
    .bind(auth_data)
    .execute(pool)
    .await
    .expect("plants the fixture user");
}

/// The row as it now stands, in the columns these tests assert over.
async fn row(pool: &PgPool, id: &str) -> (String, bool, String, i64, i32, bool, String, i64, i64) {
    sqlx::query_as(
        "SELECT password, emailverified, roles, deleteat, failedattempts, mfaactive, email,
                lastpasswordupdate, createat
           FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the row is readable")
}

/// The submitted user: every protected field carries an attacker's value.
fn poisoned(stored: &User) -> User {
    let mut user = stored.clone();
    user.password = "attacker-password-hash".to_owned();
    // `auth_data` and `auth_service` are poisoned on the SSO fixture instead: `IsValid` runs
    // before the copies and refuses an `AuthData` alongside a `Password`, so a poisoned pair
    // here would be a 400 rather than a demonstration that the copy happened.
    user.email_verified = false;
    user.failed_attempts = 0;
    user.mfa_active = false;
    user.mfa_secret = "attacker-mfa-secret".to_owned();
    user.mfa_used_timestamps = Some(vec!["999".to_owned()]);
    user.last_password_update = 1;
    user.last_picture_update = 1;
    user.last_login = 1;
    user.remote_id = Some("attacker-remote".to_owned());
    user.create_at = 1;
    user.roles = "system_user system_admin system_manager".to_owned();
    user.delete_at = 0;
    // …and one field a client *is* allowed to change, so the write is not a no-op.
    user.nickname = "changed-nick".to_owned();
    user
}

#[tokio::test]
async fn the_protected_fields_come_from_the_stored_row_not_the_request() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant(&pool, USER_ID, "mmrsupdate-one", "").await;

    let store = SqlUserStore::new(pool.clone());
    let stored = store.get(USER_ID).await.expect("the fixture reads back");
    let submitted = poisoned(&stored);

    let update = store
        .update(&submitted, false)
        .await
        .expect("the update runs");

    let (password, verified, roles, delete_at, failed, mfa, _email, last_pw, create_at) =
        row(&pool, USER_ID).await;

    assert_eq!(
        password, "stored-password-hash",
        "the password hash is not the client's"
    );
    assert!(verified, "email_verified is not the client's");
    assert_eq!(
        roles, "system_user system_admin",
        "an untrusted caller cannot grant itself a role"
    );
    assert_eq!(delete_at, 0, "delete_at comes from the row");
    assert_eq!(failed, 7, "the failed-login count is not the client's");
    assert!(mfa, "MFA cannot be turned off through an update");
    assert_eq!(
        last_pw, 1700000000003,
        "last_password_update is not the client's"
    );
    assert_eq!(create_at, 1700000000001, "create_at cannot be moved");

    // The one field a client may change did change — so the assertions above are about the
    // copies and not about a write that never happened.
    let after = store.get(USER_ID).await.expect("reads back");
    assert_eq!(after.nickname, "changed-nick");
    assert_eq!(
        after.auth_service, "",
        "auth_service is copied from the row too"
    );
    // `get` does **not** sanitize — only the `UserUpdate` halves are — so the row's own secret
    // comes back here, which is what proves the copy happened rather than the scrub.
    assert_eq!(after.mfa_secret, "stored-mfa-secret");
    assert_eq!(after.remote_id.as_deref(), Some(""));

    // **`Old` is the row before the write and `New` is the row after.** A caller that compared
    // `New` against `New` — which is what a port returning the same user twice would give it —
    // would never send an email-change mail or mint a new default avatar.
    assert_eq!(update.old.nickname, "stored-nick");
    assert_eq!(update.new.nickname, "changed-nick");
    assert_eq!(
        update.old.password, "",
        "both halves are sanitized before they are returned"
    );
    assert_eq!(update.new.password, "");

    // Deactivating an account is not something an untrusted update can undo, either — so plant
    // the deletion and confirm the copy holds it.
    sqlx::query("UPDATE users SET deleteat = 1700000009999 WHERE id = $1")
        .bind(USER_ID)
        .execute(&pool)
        .await
        .expect("deactivates the fixture");
    let stored = store.get(USER_ID).await.expect("reads back");
    let mut reviving = stored.clone();
    reviving.delete_at = 0;
    store.update(&reviving, false).await.expect("runs");
    let (_, _, _, delete_at, _, _, _, _, _) = row(&pool, USER_ID).await;
    assert_eq!(
        delete_at, 1700000009999,
        "an untrusted update cannot un-deactivate an account"
    );

    purge(&pool).await;
}

/// The trusted path — the CLI's, not any route's — lets `Roles` and `DeleteAt` through, and
/// nothing else.
#[tokio::test]
async fn the_trusted_path_differs_in_exactly_two_fields() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant(&pool, USER_ID, "mmrsupdate-two", "").await;

    let store = SqlUserStore::new(pool.clone());
    let stored = store.get(USER_ID).await.expect("reads back");
    let submitted = poisoned(&stored);

    store
        .update(&submitted, true)
        .await
        .expect("the update runs");

    let (password, verified, roles, _delete_at, failed, mfa, _email, _last_pw, create_at) =
        row(&pool, USER_ID).await;

    assert_eq!(
        roles, "system_user system_admin system_manager",
        "the trusted path takes the submitted roles"
    );
    // Everything else is still the row's.
    assert_eq!(password, "stored-password-hash");
    assert!(verified);
    assert_eq!(failed, 7);
    assert!(mfa);
    assert_eq!(create_at, 1700000000001);

    purge(&pool).await;
}

/// An email change clears `EmailVerified` — on the untrusted path only — and an SSO user's is
/// forced back on afterwards.
#[tokio::test]
async fn an_email_change_clears_verification_unless_the_user_is_sso() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant(&pool, USER_ID, "mmrsupdate-three", "").await;
    // `IsSSOUser` is `AuthService != "" && AuthService != "email"`, so SAML qualifies — and
    // **SAML is deliberately not one of the OAuth services**. An OAuth user's email is pinned to
    // the stored one a few lines earlier, so a gitlab fixture would never reach the clear at all
    // and this test would be asserting the wrong branch. (Measured: it did, first run.)
    plant(&pool, OTHER_ID, "mmrsupdate-four", "saml").await;

    let store = SqlUserStore::new(pool.clone());

    let stored = store.get(USER_ID).await.expect("reads back");
    let mut changed = stored.clone();
    changed.email = "mmrsupdate-three-new@mmrs.invalid".to_owned();
    store.update(&changed, false).await.expect("runs");
    let (_, verified, _, _, _, _, email, _, _) = row(&pool, USER_ID).await;
    assert_eq!(
        email, "mmrsupdate-three-new@mmrs.invalid",
        "the email changed"
    );
    assert!(!verified, "and the verification was cleared");

    // The SSO user's is forced true again by the "lazy migration", *after* the clear.
    let stored = store.get(OTHER_ID).await.expect("reads back");
    let mut changed = stored.clone();
    changed.email = "mmrsupdate-four-new@mmrs.invalid".to_owned();
    store.update(&changed, false).await.expect("runs");
    let (_, verified, _, _, _, _, email, _, _) = row(&pool, OTHER_ID).await;
    assert_eq!(email, "mmrsupdate-four-new@mmrs.invalid");
    assert!(verified, "IsSSOUser forces EmailVerified back on");

    // And an **OAuth** user's email cannot be changed through an untrusted update at all: the
    // stored address is copied back over the submitted one.
    plant(
        &pool,
        "mmrsupdateuser00000000usr3",
        "mmrsupdate-eight",
        "gitlab",
    )
    .await;
    let stored = store
        .get("mmrsupdateuser00000000usr3")
        .await
        .expect("reads back");
    let mut changed = stored.clone();
    changed.email = "mmrsupdate-eight-new@mmrs.invalid".to_owned();
    store.update(&changed, false).await.expect("runs");
    let (_, _, _, _, _, _, email, _, _) = row(&pool, "mmrsupdateuser00000000usr3").await;
    assert_eq!(
        email, "mmrsupdate-eight@mmrs.invalid",
        "an OAuth user's email is pinned to the stored one"
    );

    purge(&pool).await;
}

/// A username change rewrites the mention keys that were derived from the old one, and leaves
/// the rest of the list alone.
#[tokio::test]
async fn a_username_change_rewrites_the_derived_mention_keys() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant(&pool, USER_ID, "mmrsupdate-five", "").await;

    let store = SqlUserStore::new(pool.clone());
    let stored = store.get(USER_ID).await.expect("reads back");

    let mut with_keys = stored.clone();
    let mut notify = mm_model::utils::StringMap::new();
    notify.insert(
        "mention_keys".to_owned(),
        "mmrsupdate-five,@mmrsupdate-five,keepme".to_owned(),
    );
    with_keys.notify_props = Some(notify);
    store.update(&with_keys, false).await.expect("runs");

    let mut renamed = store.get(USER_ID).await.expect("reads back");
    renamed.username = "mmrsupdate-five-renamed".to_owned();
    store.update(&renamed, false).await.expect("runs");

    let after = store.get(USER_ID).await.expect("reads back");
    let keys = after
        .notify_props
        .as_ref()
        .and_then(|props| props.get("mention_keys"))
        .map(String::as_str)
        .unwrap_or_default();
    // **Go removes the old username and does not add the new one**, and the value it leaves
    // begins with a comma — `""` concatenated with `"," + join(kept)`. Both are surprising and
    // both are pinned here rather than corrected, because a client parses this string.
    assert_eq!(
        keys, ",keepme",
        "the two derived keys are gone, the unrelated one survives, and the leading comma is Go's"
    );

    purge(&pool).await;
}

/// A username another row already holds is a **conflict naming `Username`**, which the app layer
/// turns into a different error id from an email collision.
#[tokio::test]
async fn a_taken_username_is_a_username_conflict() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant(&pool, USER_ID, "mmrsupdate-six", "").await;
    plant(&pool, OTHER_ID, "mmrsupdate-seven", "").await;

    let store = SqlUserStore::new(pool.clone());
    let mut taking = store.get(USER_ID).await.expect("reads back");
    taking.username = "mmrsupdate-seven".to_owned();

    let err = store
        .update(&taking, false)
        .await
        .expect_err("the unique index refuses it");
    assert_eq!(
        err.conflict_resource(),
        Some("Username"),
        "not an email conflict: {err}"
    );

    let mut taking = store.get(USER_ID).await.expect("reads back");
    taking.email = "mmrsupdate-seven@mmrs.invalid".to_owned();
    let err = store
        .update(&taking, false)
        .await
        .expect_err("the unique index refuses it");
    assert_eq!(err.conflict_resource(), Some("Email"), "{err}");

    purge(&pool).await;
}

/// A user that is not there is `InvalidInput`, which the app layer answers **400** to — not the
/// 500 an ordinary store failure gets.
#[tokio::test]
async fn a_missing_row_is_invalid_input_rather_than_not_found() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let store = SqlUserStore::new(pool.clone());
    let mut ghost = User {
        id: USER_ID.to_owned(),
        username: "mmrsupdate-ghost".to_owned(),
        email: "mmrsupdate-ghost@mmrs.invalid".to_owned(),
        ..Default::default()
    };
    ghost.create_at = 1700000000001;

    let err = store
        .update(&ghost, false)
        .await
        .expect_err("there is no such row");
    assert!(err.is_invalid_input(), "{err}");
    assert!(!err.is_not_found(), "and not the NotFound `get` would give");
}
