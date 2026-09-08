//! `App::session_has_permission_to_user_or_bot`, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_user_or_bot
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! The function resolves "or bot" by *trying* the bot path and reading its failure: **only**
//! `store.sql_bot.get.missing.app_error` from `SqlBotStore.Get` falls through to the user check.
//! A refusal that hides an existing bot carries the same id with a different `where`, and must not
//! fall through — otherwise a caller holding `edit_other_users` could read every bot's personal
//! access tokens.
//!
//! Over HTTP the two answers are the same 403, because the routes that call this
//! (`getUserAccessTokensForUser`, `getUserAccessToken`) refuse either way when the caller cannot
//! reach the bot. A mutation that widened the guard to *any* error therefore survived the whole
//! parity suite. Here the function returns a `bool` and the difference is visible.
//!
//! Every test is named `user_or_bot_*` so `MUTATE_FILTER` can select them by name.

use mm_app::App;
use mm_model::session::Session;
use mm_store::SqlStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One mutable database, shared with the other `mm-app` suites.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CALLER: &str = "mmrsuobcallerxxxxxxxxxxxxx";
const PLAIN_TARGET: &str = "mmrsuobtargetxxxxxxxxxxxxx";
const BOT_USER: &str = "mmrsuobbotuserxxxxxxxxxxxx";
const BOT_OWNER: &str = "mmrsuobbotownerxxxxxxxxxxx";

const EDITOR_ROLE: &str = "mmrs_uob_editor";
/// `edit_other_users` **and** `read_others_bots`, and *not* `manage_others_bots`. That is the only
/// combination for which the bot path refuses with a permission error while the user path would
/// grant — which is exactly the pair the guard has to keep apart.
const EDITOR_PERMISSIONS: &str = "edit_other_users read_others_bots";

/// `edit_other_users` alone: the bot path then refuses with the **existence-hiding 404**, which
/// also must not fall through.
const BLIND_ROLE: &str = "mmrs_uob_blind";
const BLIND_PERMISSIONS: &str = "edit_other_users";

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
        "DELETE FROM bots WHERE userid LIKE 'mmrsuob%'",
        "DELETE FROM users WHERE id LIKE 'mmrsuob%'",
        "DELETE FROM roles WHERE name LIKE 'mmrs\\_uob\\_%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover fixtures");
    }
}

/// Four users and two roles. `nickname`/`firstname`/`lastname` are `''` rather than NULL for the
/// reason `db_authorization` records: a NULL there makes the row match every user search.
async fn seed(pool: &PgPool) {
    purge(pool).await;

    for (id, username, roles) in [
        (CALLER, "mmrsuobcaller", "system_user"),
        (PLAIN_TARGET, "mmrsuobtarget", "system_user"),
        (BOT_USER, "mmrsuobbotuser", "system_user"),
        (BOT_OWNER, "mmrsuobbotowner", "system_user"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO users (id, createat, updateat, deleteat, username, email, emailverified,
                               password, authdata, authservice, roles, allowmarketing, props,
                               notifyprops, lastpasswordupdate, failedattempts, locale, mfaactive,
                               mfasecret, position, timezone, remoteid, lastlogin,
                               nickname, firstname, lastname)
            VALUES ($1, 1755000000000, 1755000000000, 0, $2, $2 || '@mmrs.invalid', true,
                    '', NULL, '', $3, false, 'null'::jsonb, 'null'::jsonb, 1755000000000, 0, 'en',
                    false, '', '', 'null'::jsonb, NULL, 0, '', '', '')
            "#,
        )
        .bind(id)
        .bind(username)
        .bind(roles)
        .execute(pool)
        .await
        .expect("inserts a test user");
    }

    sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat,
                           lasticonupdate)
         VALUES ($1, 'mmrs user-or-bot fixture', $2, 1755000000000, 1755000000000, 0, 0)",
    )
    .bind(BOT_USER)
    .bind(BOT_OWNER)
    .execute(pool)
    .await
    .expect("inserts the bot");

    for (name, permissions) in [
        (EDITOR_ROLE, EDITOR_PERMISSIONS),
        (BLIND_ROLE, BLIND_PERMISSIONS),
    ] {
        sqlx::query(
            r#"
            INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                               permissions, schememanaged, builtin, schemeid)
            VALUES ('mmrsuobrole' || substr(md5($1), 1, 15), $1, 'MMRS user-or-bot',
                    'inserted by mm-app tests', 1755000000000, 1755000000000, 0, $2,
                    false, false, NULL)
            "#,
        )
        .bind(name)
        .bind(permissions)
        .execute(pool)
        .await
        .expect("inserts a test role");
    }
}

fn session(roles: &str) -> Session {
    Session {
        user_id: CALLER.to_owned(),
        roles: roles.to_owned(),
        ..Default::default()
    }
}

async fn app(pool: &PgPool) -> App {
    App::new(SqlStore::from_pool(pool.clone()))
}

/// **The finding.** A caller who may *see* the bot but not manage it is refused — and that refusal
/// must not become a grant through the user path, even though `edit_other_users` would grant it.
#[tokio::test]
async fn user_or_bot_does_not_fall_through_on_a_permission_refusal() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool).await;
    let session = session(&format!("system_user {EDITOR_ROLE}"));

    // The same permissions grant the *user* path outright, which is what makes the bot answer
    // meaningful rather than a blanket denial.
    assert!(
        app.session_has_permission_to_user_or_bot(&session, PLAIN_TARGET)
            .await,
        "edit_other_users carries an ordinary user"
    );

    assert!(
        !app.session_has_permission_to_user_or_bot(&session, BOT_USER)
            .await,
        "a permission refusal on the bot is final — falling through here would hand this caller \
         every bot's access tokens"
    );

    // The refusal really is the permission one and not the existence-hiding 404: with
    // `read_others_bots` the caller can see that the bot exists.
    let err = app
        .session_has_permission_to_manage_bot(&session, BOT_USER)
        .await
        .expect_err("manage is refused");
    assert_eq!(err.id, "api.context.permissions.app_error");
}

/// The **other** refusal — no `read_others_bots`, so the bot path hides the bot behind its own
/// 404. That id is the one the guard matches on, and it still must not fall through, because the
/// `where` is `permissions` rather than `SqlBotStore.Get`.
#[tokio::test]
async fn user_or_bot_does_not_fall_through_on_the_hidden_bot_404() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool).await;
    let session = session(&format!("system_user {BLIND_ROLE}"));

    let err = app
        .session_has_permission_to_manage_bot(&session, BOT_USER)
        .await
        .expect_err("manage is refused");
    assert_eq!(
        err.id, "store.sql_bot.get.missing.app_error",
        "the same id a genuine miss carries"
    );
    assert_eq!(
        err.where_, "permissions",
        "and a different `where` — which is the whole discriminator"
    );

    assert!(
        !app.session_has_permission_to_user_or_bot(&session, BOT_USER)
            .await,
        "matching on the id alone would let this through"
    );
    assert!(
        app.session_has_permission_to_user_or_bot(&session, PLAIN_TARGET)
            .await,
        "the same caller passes on an ordinary user"
    );
}

/// The fall-through that **is** correct: an id that names no bot at all reaches the user check.
#[tokio::test]
async fn user_or_bot_falls_through_for_an_ordinary_user() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool).await;

    let err = app
        .session_has_permission_to_manage_bot(&session("system_user"), PLAIN_TARGET)
        .await
        .expect_err("no such bot");
    assert_eq!(err.id, "store.sql_bot.get.missing.app_error");
    assert_eq!(
        err.where_, "SqlBotStore.Get",
        "from the store, not the gate"
    );

    // Without `edit_other_users` the user check denies, so the fall-through is reached and its
    // answer is the user path's — not a blanket grant.
    assert!(
        !app.session_has_permission_to_user_or_bot(&session("system_user"), PLAIN_TARGET)
            .await
    );
    // And the caller's own id passes on the self branch.
    assert!(
        app.session_has_permission_to_user_or_bot(&session("system_user"), CALLER)
            .await
    );
}

/// A bot the caller **owns**, with `manage_bots`, is a grant rather than a fall-through — the
/// first arm of `SessionHasPermissionToManageBot`.
#[tokio::test]
async fn user_or_bot_grants_an_owned_bot_with_manage_bots() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    // Re-own the bot to the caller.
    sqlx::query("UPDATE bots SET ownerid = $1 WHERE userid = $2")
        .bind(CALLER)
        .bind(BOT_USER)
        .execute(&pool)
        .await
        .expect("re-owns the bot");

    let app = app(&pool).await;

    // `system_user` grants neither `manage_bots` nor `read_bots` on a stock server, so an owner
    // without them is still hidden from themselves — Go's comment calls this "kind of silly".
    assert!(
        !app.session_has_permission_to_user_or_bot(&session("system_user"), BOT_USER)
            .await
    );

    // `system_admin` holds `manage_bots`, so the owner branch grants.
    assert!(
        app.session_has_permission_to_user_or_bot(&session("system_user system_admin"), BOT_USER)
            .await,
        "an owner with manage_bots is granted by the bot path itself"
    );
}
