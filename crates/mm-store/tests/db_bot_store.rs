//! `SqlBotStore`'s three filters, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_bot_store
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! All three of `GetAll`'s filters are unreachable through `GET /api/v4/bots` on this deployment,
//! and each for its own reason:
//!
//! | filter | why no HTTP request reaches it |
//! |---|---|
//! | `OwnerId` | the handler sets it to the caller's id only for a caller with `read_bots` and **not** `read_others_bots`; every stock role granting the first grants the second, so it is always `""` |
//! | `IncludeDeleted` | no bot on this deployment has a non-zero `DeleteAt`, so both settings return the same rows |
//! | `OnlyOrphaned` | no bot's owner is a deleted user, so it always returns `[]` |
//!
//! That is three predicates a mutation can delete with no test noticing — the same shape as
//! `db_webhook_owner_filter.rs`, which exists for the same reason. The rows are planted here.
//!
//! # `OnlyOrphaned` is an inner join, and one bot on this deployment proves it matters
//!
//! Go joins `Users o ON o.Id = b.OwnerId`, which **drops** a bot whose owner is not a user at all.
//! The `calls` bot's `OwnerId` is `com.mattermost.calls` — a plugin id. The Rust port uses a left
//! join plus `o.Id IS NOT NULL` so that the compile-time-checked query can carry all three filters
//! as parameters; without the null check, a plugin-owned bot would pass the orphan filter that
//! Go's inner join excludes it from. `bot_store_only_orphaned_excludes_a_plugin_owner` is that
//! test, and it fails if the null check is removed.
//!
//! # Every test here is named `bot_store_*`
//!
//! `MUTATE_FILTER` selects test **names**, not files. A filter naming this file matches nothing,
//! cargo runs zero tests, and every mutation is reported SURVIVED.

use mm_model::bot::BotGetOptions;
use mm_store::{BotStore, SqlBotStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const OWNER_LIVE: &str = "mmrsbotowner0000000000live";
const OWNER_GONE: &str = "mmrsbotowner0000000000gone";
const OWNER_PLUGIN: &str = "com.mattermost.mmrsbotstore";

const BOT_LIVE: &str = "mmrsbot00000000000000live1";
const BOT_OTHER: &str = "mmrsbot0000000000000other1";
const BOT_DELETED: &str = "mmrsbot00000000000000del01";
const BOT_ORPHAN: &str = "mmrsbot0000000000000orph01";
const BOT_PLUGIN: &str = "mmrsbot0000000000000plug01";

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
    for statement in [
        "DELETE FROM bots WHERE userid LIKE 'mmrsbot%'",
        "DELETE FROM users WHERE id LIKE 'mmrsbot%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Both an owner and a bot need a `Users` row: the bot's, because `GetAll` joins it for the
/// username, and the owner's, because `OnlyOrphaned` joins it for the `DeleteAt`.
async fn plant_user(pool: &PgPool, id: &str, username: &str, first_name: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users
            (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
             emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
             notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
             mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
         VALUES ($1, 1788600000000, 1788600000000, $4, $2, '', NULL, '', $2 || '@mmrs.invalid',
                 false, '', $3, '', '', 'system_user', false, '{}'::jsonb, '{}'::jsonb,
                 1788600000000, 0, 0, 'en', '{}'::jsonb, false, '', NULL, 0, '{}')",
    )
    .bind(id)
    .bind(username)
    .bind(first_name)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("the user row is written");
}

/// Five bots: two live under different owners, one soft-deleted, one whose owner is a deleted
/// user, and one owned by a plugin id that is not a user at all.
///
/// `CreateAt` is staggered so `ORDER BY b.CreateAt ASC` has something to order.
async fn seed(pool: &PgPool) {
    plant_user(pool, OWNER_LIVE, "mmrsbotownerlive", "Live Owner", 0).await;
    plant_user(
        pool,
        OWNER_GONE,
        "mmrsbotownergone",
        "Gone Owner",
        1788600009999,
    )
    .await;

    for (id, username, display, owner, create_at, delete_at) in [
        (
            BOT_LIVE,
            "mmrsbotlive",
            "Live Bot",
            OWNER_LIVE,
            1788600000001_i64,
            0_i64,
        ),
        (
            BOT_OTHER,
            "mmrsbotother",
            "Other Bot",
            OWNER_GONE,
            1788600000002,
            0,
        ),
        (
            BOT_DELETED,
            "mmrsbotdeleted",
            "Deleted Bot",
            OWNER_LIVE,
            1788600000003,
            1788600001000,
        ),
        (
            BOT_ORPHAN,
            "mmrsbotorphan",
            "Orphan Bot",
            OWNER_GONE,
            1788600000004,
            0,
        ),
        (
            BOT_PLUGIN,
            "mmrsbotplugin",
            "Plugin Bot",
            OWNER_PLUGIN,
            1788600000005,
            0,
        ),
    ] {
        plant_user(pool, id, username, display, delete_at).await;
        sqlx::query(
            "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat,
                               lasticonupdate)
             VALUES ($1, $2, $3, $4, $4, $5, 0)",
        )
        .bind(id)
        .bind(format!("{display} description"))
        .bind(owner)
        .bind(create_at)
        .bind(delete_at)
        .execute(pool)
        .await
        .expect("the bot row is written");
    }
}

/// Only this fixture's bots, in the order the store returned them.
fn ours(list: &mm_model::bot::BotList) -> Vec<&str> {
    list.0
        .iter()
        .filter(|bot| bot.user_id.starts_with("mmrsbot"))
        .map(|bot| bot.user_id.as_str())
        .collect()
}

fn options() -> BotGetOptions {
    BotGetOptions {
        owner_id: String::new(),
        include_deleted: false,
        only_orphaned: false,
        page: 0,
        per_page: 200,
    }
}

/// The owner filter, which no HTTP request can reach.
#[tokio::test]
async fn bot_store_narrows_to_one_owner() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlBotStore::new(pool.clone());

    let all = store.get_all(&options()).await.expect("queries");
    assert_eq!(
        ours(&all),
        vec![BOT_LIVE, BOT_OTHER, BOT_ORPHAN, BOT_PLUGIN],
        "no filter: every undeleted bot, oldest first"
    );

    let mine = store
        .get_all(&BotGetOptions {
            owner_id: OWNER_LIVE.to_owned(),
            ..options()
        })
        .await
        .expect("queries");
    assert_eq!(ours(&mine), vec![BOT_LIVE], "only the live owner's bot");

    let theirs = store
        .get_all(&BotGetOptions {
            owner_id: OWNER_GONE.to_owned(),
            ..options()
        })
        .await
        .expect("queries");
    assert_eq!(ours(&theirs), vec![BOT_OTHER, BOT_ORPHAN]);

    // An owner nothing names is empty, not everything — the filter is applied, not ignored.
    let none = store
        .get_all(&BotGetOptions {
            owner_id: "mmrsbotowner0000000000none".to_owned(),
            ..options()
        })
        .await
        .expect("queries");
    assert!(ours(&none).is_empty());

    purge(&pool).await;
}

/// `IncludeDeleted` **widens**: it drops the clause rather than selecting deleted bots.
#[tokio::test]
async fn bot_store_include_deleted_widens_rather_than_selects() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlBotStore::new(pool.clone());

    let without = store.get_all(&options()).await.expect("queries");
    assert!(!ours(&without).contains(&BOT_DELETED));

    let with = store
        .get_all(&BotGetOptions {
            include_deleted: true,
            ..options()
        })
        .await
        .expect("queries");
    assert_eq!(
        ours(&with),
        vec![BOT_LIVE, BOT_OTHER, BOT_DELETED, BOT_ORPHAN, BOT_PLUGIN],
        "the deleted bot joins the others rather than replacing them"
    );

    // `Get` carries the same flag with the same meaning.
    assert!(
        store.get(BOT_DELETED, false).await.is_err(),
        "a deleted bot is not found without the flag"
    );
    let found = store
        .get(BOT_DELETED, true)
        .await
        .expect("found with the flag");
    assert_eq!(found.delete_at, 1788600001000);
    assert_eq!(found.username, "mmrsbotdeleted");
    assert_eq!(
        found.display_name, "Deleted Bot",
        "display_name is Users.FirstName, not the username"
    );

    purge(&pool).await;
}

/// `OnlyOrphaned` selects bots whose **owner** is a deleted user — and excludes a bot whose owner
/// is not a user at all, which Go's inner join does for free and the parameterised rewrite has to
/// state.
#[tokio::test]
async fn bot_store_only_orphaned_excludes_a_plugin_owner() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlBotStore::new(pool.clone());

    let orphans = store
        .get_all(&BotGetOptions {
            only_orphaned: true,
            ..options()
        })
        .await
        .expect("queries");
    assert_eq!(
        ours(&orphans),
        vec![BOT_OTHER, BOT_ORPHAN],
        "both bots owned by the deleted user, and neither the live owner's nor the plugin's"
    );

    // The two flags compose: the deleted bot's owner is live, so widening does not add it here.
    let both = store
        .get_all(&BotGetOptions {
            only_orphaned: true,
            include_deleted: true,
            ..options()
        })
        .await
        .expect("queries");
    assert_eq!(ours(&both), vec![BOT_OTHER, BOT_ORPHAN]);

    purge(&pool).await;
}

/// The order and the page window. `ORDER BY b.CreateAt ASC` is **oldest first**, which is the
/// opposite of every other list route in this project.
#[tokio::test]
async fn bot_store_pages_oldest_first() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlBotStore::new(pool.clone());

    let by_owner = |page: i32, per_page: i32| BotGetOptions {
        owner_id: OWNER_GONE.to_owned(),
        page,
        per_page,
        ..options()
    };

    let first = store.get_all(&by_owner(0, 1)).await.expect("queries");
    assert_eq!(
        ours(&first),
        vec![BOT_OTHER],
        "createat 2 before createat 4"
    );

    let second = store.get_all(&by_owner(1, 1)).await.expect("queries");
    assert_eq!(ours(&second), vec![BOT_ORPHAN]);

    let past = store.get_all(&by_owner(9, 1)).await.expect("queries");
    assert!(ours(&past).is_empty());

    purge(&pool).await;
}

/// `Get` on a bot that is not there is `NotFound`, which the app layer turns into the 404 that
/// doubles as the permission refusal.
#[tokio::test]
async fn bot_store_get_reports_a_miss() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlBotStore::new(pool.clone());

    let found = store.get(BOT_LIVE, false).await.expect("the row exists");
    assert_eq!(found.owner_id, OWNER_LIVE);
    assert_eq!(found.description, "Live Bot description");

    let err = store
        .get("mmrsbotzzzzzzzzzzzzzzzzzzz", false)
        .await
        .expect_err("no such row");
    assert!(err.is_not_found());

    purge(&pool).await;
}
