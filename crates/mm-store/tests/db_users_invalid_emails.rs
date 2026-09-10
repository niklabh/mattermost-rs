//! `SqlUserStore::get_users_with_invalid_emails` and `get_by_auth_data` against planted rows.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_users_invalid_emails
//! ```
//!
//! # Why the query lives here and not only in the parity suite
//!
//! The route above `get_users_with_invalid_emails` refuses with a **400** whenever
//! `TeamSettings.EnableOpenServer` is on, and the development stack's Go server pins that
//! variable **on** through the environment (`scripts/go-server.sh`) — an override, so it never
//! reaches the configuration document either server could be told to read. The 200 path is
//! therefore unreachable through Go on this stack, and a cross-server comparison of it is not
//! available at any price short of a second Go server on a second configuration.
//!
//! So the *route's* refusal is tested against Go in `mm-api`'s `user_lookups` parity suite, and
//! the *query* — five predicates and a page — is tested here, against rows this file plants and
//! removes. Read the four `WHERE` clauses out of `user_store.go:2404` and each one has a row on
//! either side of it below.

use mm_store::{SqlUserStore, StoreError, UserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

/// These tests share one set of planted rows and each one seeds and purges, so they must not
/// overlap — the same guard `db_bot_store` uses, and for the same reason: `cargo test` runs the
/// tests in a binary in parallel, and the second `seed` hits `users_pkey`.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PREFIX: &str = "mmrsinvemail";

/// Ordinary, active, no auth service: reported.
const PLAIN: &str = "mmrsinvemailplain000000001";
/// The same, on a different email domain — the only one a domain filter keeps.
const ELSEWHERE: &str = "mmrsinvemailelsewhere00001";
/// `Roles = 'system_guest'` exactly: excluded.
const GUEST: &str = "mmrsinvemailguest000000001";
/// `Roles = 'system_guest system_user'`: **not** excluded, because Go compares the whole column.
const GUESTISH: &str = "mmrsinvemailguestish000001";
/// `DeleteAt > 0`: excluded.
const DEACTIVATED: &str = "mmrsinvemaildeleted0000001";
/// `AuthService = 'ldap'`: excluded — they did not choose their email.
const FEDERATED: &str = "mmrsinvemailldap0000000001";
/// A bot: excluded by the anti-join.
const BOT: &str = "mmrsinvemailbot00000000001";

/// The `AuthData` planted on [`PLAIN`], for `get_by_auth_data`.
const AUTH_DATA: &str = "mmrs-invemail-auth-data";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM bots WHERE userid LIKE 'mmrsinvemail%'",
        "DELETE FROM users WHERE id LIKE 'mmrsinvemail%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// One `Users` row.
///
/// The password, the MFA secret and `LastLogin` are all **non-empty**, which is the whole point:
/// `Sanitize` blanks exactly those three, and against a row where they were already zero the
/// sanitised and unsanitised answers are identical. A mutation deleting the `sanitize` call
/// survived until this fixture carried values for it to remove.
///
/// `MfaUsedTimestamps` is `'null'::jsonb`, never `'{}'`: Go scans it into a `model.StringArray`,
/// and an object there makes **Go** answer 500 to any `GET /api/v4/users` returning the row —
/// a failure in a different test binary, caused by a fixture in this one.
#[expect(clippy::too_many_arguments, reason = "one argument per planted column")]
async fn plant_user(
    pool: &PgPool,
    id: &str,
    username: &str,
    email: &str,
    roles: &str,
    auth_service: &str,
    auth_data: Option<&str>,
    delete_at: i64,
) {
    sqlx::query(
        "INSERT INTO users
            (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
             emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
             notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
             mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
         VALUES ($1, 1788600000000, 1788600000000, $7, $2, 'planted-password-hash', $6, $5, $3,
                 false, '', '', '', '', $4, false, '{}'::jsonb, '{}'::jsonb,
                 1788600000000, 0, 0, 'en', '{}'::jsonb, false, 'planted-mfa-secret', NULL,
                 1788600001234, 'null'::jsonb)",
    )
    .bind(id)
    .bind(username)
    .bind(email)
    .bind(roles)
    .bind(auth_service)
    .bind(auth_data)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("the user row is written");
}

async fn seed(pool: &PgPool) {
    purge(pool).await;
    plant_user(
        pool,
        PLAIN,
        "mmrsinvemailplain",
        "mmrsinvemailplain@mmrs.invalid",
        "system_user",
        "",
        Some(AUTH_DATA),
        0,
    )
    .await;
    plant_user(
        pool,
        ELSEWHERE,
        "mmrsinvemailelsewhere",
        "mmrsinvemailelsewhere@elsewhere.test",
        "system_user",
        "",
        None,
        0,
    )
    .await;
    plant_user(
        pool,
        GUEST,
        "mmrsinvemailguest",
        "mmrsinvemailguest@mmrs.invalid",
        "system_guest",
        "",
        None,
        0,
    )
    .await;
    plant_user(
        pool,
        GUESTISH,
        "mmrsinvemailguestish",
        "mmrsinvemailguestish@mmrs.invalid",
        "system_guest system_user",
        "",
        None,
        0,
    )
    .await;
    plant_user(
        pool,
        DEACTIVATED,
        "mmrsinvemaildeleted",
        "mmrsinvemaildeleted@mmrs.invalid",
        "system_user",
        "",
        None,
        1788600009999,
    )
    .await;
    plant_user(
        pool,
        FEDERATED,
        "mmrsinvemailldap",
        "mmrsinvemailldap@mmrs.invalid",
        "system_user",
        "ldap",
        Some("mmrs-invemail-ldap"),
        0,
    )
    .await;
    plant_user(
        pool,
        BOT,
        "mmrsinvemailbot",
        "mmrsinvemailbot@mmrs.invalid",
        "system_user",
        "",
        None,
        0,
    )
    .await;
    sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat,
                           lasticonupdate)
         VALUES ($1, 'planted by db_users_invalid_emails', $2, 1788600000000, 1788600000000, 0, 0)",
    )
    .bind(BOT)
    .bind(PLAIN)
    .execute(pool)
    .await
    .expect("the bot row is written");
}

/// Only this file's rows, so the assertion does not depend on the rest of the database.
fn ours(users: &[mm_model::user::User]) -> Vec<&str> {
    let mut ids: Vec<&str> = users
        .iter()
        .map(|user| user.id.as_str())
        .filter(|id| id.starts_with(PREFIX))
        .collect();
    ids.sort_unstable();
    ids
}

/// The four exclusions, with a row on each side of every one.
#[tokio::test]
async fn store_invalid_emails_four_exclusions_each_keep_a_row_out() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    // No restricted domains — the stock configuration — so there is no email predicate at all
    // and "invalid" means every account the other four clauses admit.
    let users = store
        .get_users_with_invalid_emails(0, 200, "")
        .await
        .expect("the query runs");

    assert_eq!(
        ours(&users),
        vec![ELSEWHERE, GUESTISH, PLAIN],
        "guests, deactivated accounts, federated accounts and bots are all excluded"
    );

    purge(&pool).await;
}

/// `Users.Roles != 'system_guest'` is a whole-column comparison, not a substring test.
///
/// A user whose roles are `system_guest system_user` is **reported**, which is almost certainly
/// not what the clause intends — every other guest predicate in this store is a
/// `LIKE '%system_guest%'`. Reproduced because it is Go's, and pinned here because a reader
/// "fixing" it to a `LIKE` would change which accounts an administrator is told to chase.
#[tokio::test]
async fn store_invalid_emails_a_guest_with_a_second_role_is_kept() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .get_users_with_invalid_emails(0, 200, "")
        .await
        .expect("the query runs");
    let ids = ours(&users);

    assert!(ids.contains(&GUESTISH), "`!= 'system_guest'` is exact");
    assert!(!ids.contains(&GUEST), "and the bare role is excluded");

    purge(&pool).await;
}

/// The domain list: one `Email NOT LIKE '%domain%'` per non-empty piece, ANDed.
#[tokio::test]
async fn store_invalid_emails_every_domain_removes_its_own_addresses() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let only_outsiders = store
        .get_users_with_invalid_emails(0, 200, "mmrs.invalid")
        .await
        .expect("the query runs");
    assert_eq!(
        ours(&only_outsiders),
        vec![ELSEWHERE],
        "one allowed domain leaves only the address outside it"
    );

    // Two domains, and the second one takes the last row away.
    let nobody = store
        .get_users_with_invalid_emails(0, 200, "mmrs.invalid,elsewhere.test")
        .await
        .expect("the query runs");
    assert_eq!(ours(&nobody), Vec::<&str>::new(), "both domains allowed");

    // **Nothing is trimmed.** `" elsewhere.test"` — the shape a human writes after a comma —
    // becomes `LIKE '% elsewhere.test%'`, which no address matches, so the domain silently
    // allows nobody and the account is reported as invalid anyway.
    let untrimmed = store
        .get_users_with_invalid_emails(0, 200, "mmrs.invalid, elsewhere.test")
        .await
        .expect("the query runs");
    assert_eq!(
        ours(&untrimmed),
        vec![ELSEWHERE],
        "a space after the comma is part of the domain"
    );

    // Empty pieces are dropped by the loop's own guard rather than becoming `LIKE '%%'`, which
    // would match every address and report nobody.
    let empties = store
        .get_users_with_invalid_emails(0, 200, ",,mmrs.invalid,,")
        .await
        .expect("the query runs");
    assert_eq!(
        ours(&empties),
        vec![ELSEWHERE],
        "empty pieces are skipped, not treated as a wildcard"
    );

    // The match is a substring with no `@` anchor, so a *sub*domain of an allowed domain is
    // allowed too — and so is an address that merely contains the text.
    let substring = store
        .get_users_with_invalid_emails(0, 200, "invalid")
        .await
        .expect("the query runs");
    assert_eq!(
        ours(&substring),
        vec![ELSEWHERE],
        "`%invalid%` matches `@mmrs.invalid` — there is no `@` in the pattern"
    );

    purge(&pool).await;
}

/// `page * per_page` is the offset, and the pages partition the result.
///
/// There is **no `ORDER BY`**, so this asserts on the union of two pages rather than on which
/// row landed on which — the only claim the query actually supports.
#[tokio::test]
async fn store_invalid_emails_pages_partition_the_result() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let all = store
        .get_users_with_invalid_emails(0, 200, "")
        .await
        .expect("the query runs");
    let total = all.len();
    assert!(total >= 3, "the fixture contributes three rows");

    let first = store
        .get_users_with_invalid_emails(0, 2, "")
        .await
        .expect("the query runs");
    assert_eq!(first.len(), 2, "a page of two");

    let second = store
        .get_users_with_invalid_emails(1, 2, "")
        .await
        .expect("the query runs");
    assert_eq!(
        second.len(),
        std::cmp::min(2, total - 2),
        "page 1 starts at offset 2"
    );

    let mut union: Vec<&str> = first
        .iter()
        .chain(second.iter())
        .map(|user| user.id.as_str())
        .collect();
    union.sort_unstable();
    union.dedup();
    assert_eq!(union.len(), first.len() + second.len(), "no row on both");

    purge(&pool).await;
}

/// `Sanitize(map[string]bool{})` — the password and the MFA secret go, the email stays.
///
/// Blanking the email would empty the one column the route exists to show, and
/// `ClearNonProfileFields` (the *other* sanitiser, one line away in the model) does exactly that
/// for a non-admin viewer. The two are easy to confuse and only one of them is right here.
#[tokio::test]
async fn store_invalid_emails_rows_keep_their_emails() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .get_users_with_invalid_emails(0, 200, "")
        .await
        .expect("the query runs");
    let plain = users
        .iter()
        .find(|user| user.id == PLAIN)
        .expect("the plain user is reported");

    assert_eq!(
        plain.email, "mmrsinvemailplain@mmrs.invalid",
        "the email survives — it is the point of the route"
    );
    assert!(plain.password.is_empty(), "the password is blanked");
    assert!(plain.mfa_secret.is_empty(), "and the MFA secret");
    assert_eq!(plain.last_login, 0, "and LastLogin");
    assert_eq!(
        plain.auth_data.as_deref(),
        Some(AUTH_DATA),
        "an empty options map leaves the auth fields alone — this is `Sanitize`, not \
         `ClearNonProfileFields`"
    );

    purge(&pool).await;
}

/// `GetByAuthData` matches on `AuthData` alone, and an empty argument is invalid input.
#[tokio::test]
async fn store_auth_data_finds_the_row_and_refuses_an_empty_key() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let found = store
        .get_by_auth_data(AUTH_DATA)
        .await
        .expect("the planted auth data resolves");
    assert_eq!(found.id, PLAIN);

    // **No `AuthService` predicate**, which is the difference from the neighbouring `GetByAuth`:
    // the federated user's auth data resolves without naming `ldap`.
    let federated = store
        .get_by_auth_data("mmrs-invemail-ldap")
        .await
        .expect("an auth service is not required");
    assert_eq!(federated.id, FEDERATED);

    let missing = store.get_by_auth_data("mmrs-no-such-auth-data").await;
    assert!(
        matches!(missing, Err(StoreError::NotFound { .. })),
        "a miss is not found, which the app answers 404 to"
    );

    let empty = store.get_by_auth_data("").await;
    assert!(
        matches!(empty, Err(StoreError::InvalidInput { .. })),
        "an empty key is invalid input, which the app answers **400** to — a different status \
         from the miss above, under the same error id"
    );

    purge(&pool).await;
}
