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
use mm_store::{BotStore, SqlBotStore, UserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const OWNER_LIVE: &str = "mmrsbotowner0000000000live";
const OWNER_GONE: &str = "mmrsbotowner0000000000gone";
const OWNER_PLUGIN: &str = "com.mattermost.mmrsbotstore";

const BOT_LIVE: &str = "mmrsbotstore0000000000live";
const BOT_OTHER: &str = "mmrsbotstore000000000other";
const BOT_DELETED: &str = "mmrsbotstore0000000delete1";
const BOT_ORPHAN: &str = "mmrsbotstore00000000orphan";
const BOT_PLUGIN: &str = "mmrsbotstore00000000plugin";

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

/// **Scoped to this file's own ids, not to the whole `mmrsbot%` prefix.**
///
/// `common::plant_bot` in the parity binary writes `mmrsbot` followed by a left-zero-padded tag,
/// so every id it mints has a `0` in position 8 and none can collide with `mmrsbotstore…` or
/// `mmrsbotowner…`. A blanket `mmrsbot%` sweep here deletes that binary's fixtures instead —
/// from a *different process*, which no mutex in either can serialise. It ran for several
/// sessions before the bot write suite doubled the number of planted rows and made the window
/// worth closing.
async fn purge(pool: &PgPool) {
    for statement in [
        // The write tests go through `Save`, which **mints** the id — so there is no prefix on
        // the id to sweep and the username is the only handle. A created bot that outlives its
        // test takes its username with it, and every later run of that test then fails on the
        // unique constraint. Measured: four did.
        "DELETE FROM bots WHERE userid IN (SELECT id FROM users WHERE username LIKE 'mmrsbotstore%')",
        "DELETE FROM users WHERE username LIKE 'mmrsbotstore%'",
        "DELETE FROM bots WHERE userid LIKE 'mmrsbotstore%' OR userid LIKE 'mmrsbotowner%'",
        "DELETE FROM users WHERE id LIKE 'mmrsbotstore%' OR id LIKE 'mmrsbotowner%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Both an owner and a bot need a `Users` row: the bot's, because `GetAll` joins it for the
/// username, and the owner's, because `OnlyOrphaned` joins it for the `DeleteAt`.
///
/// `MfaUsedTimestamps` is `'null'::jsonb` and not `'{}'`: Go scans that column into a
/// `model.StringArray`, so an object there makes **Go** answer 500 to any `GET /api/v4/users`
/// whose page contains the row — a failure in the parity binary, which runs concurrently with
/// this one, caused by a fixture in this one.
async fn plant_user(pool: &PgPool, id: &str, username: &str, first_name: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users
            (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
             emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
             notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
             mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
         VALUES ($1, 1788600000000, 1788600000000, $4, $2, '', NULL, '', $2 || '@mmrs.invalid',
                 false, '', $3, '', '', 'system_user', false, '{}'::jsonb, '{}'::jsonb,
                 1788600000000, 0, 0, 'en', '{}'::jsonb, false, '', NULL, 0, 'null'::jsonb)",
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
        // `mmrsbotstore`, not `mmrsbot`: the parity binary's own planted bots share the shorter
        // prefix and would otherwise drift into this file's ordering assertions.
        .filter(|bot| bot.user_id.starts_with("mmrsbotstore"))
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

// =================================================================================================
// The write half: `Save`, `Update`, and the `Users` pair `App.CreateBot` drives them through.
// =================================================================================================

/// The store needs a [`mm_model::user::UserPasswordHasher`] and the real ones live in `mm-app`,
/// which this crate cannot depend on. A bot's password is empty, so `PreSave` never calls this —
/// and that is itself worth pinning: if it ever *is* called for a bot, this panics in a test
/// rather than writing an unhashed password in production.
struct NoHasher;

impl mm_model::user::UserPasswordHasher for NoHasher {
    fn hash(&self, _password: &str) -> Result<String, mm_model::user::PasswordHashError> {
        panic!("a bot has no password; PreSave must not reach the hasher")
    }
}

const WRITE_OWNER: &str = "mmrsbotowner000000000write";

/// `App.CreateBot`'s two inserts, in its order, without the app layer.
async fn create_bot(
    users: &mm_store::user_store::SqlUserStore,
    bots: &SqlBotStore,
    username: &str,
    display_name: &str,
    description: &str,
) -> Result<mm_model::bot::Bot, mm_store::StoreError> {
    let mut bot = mm_model::bot::Bot {
        username: username.to_owned(),
        display_name: display_name.to_owned(),
        description: description.to_owned(),
        owner_id: WRITE_OWNER.to_owned(),
        ..Default::default()
    };
    let user = users
        .save(&mm_model::bot::user_from_bot(&bot), &NoHasher)
        .await?;
    bot.user_id = user.id;
    bots.save(&bot).await
}

/// A create writes both rows, and the bot that comes back is the one the reads will see.
///
/// The three stamps `PreSave` applies are asserted individually: `CreateAt == UpdateAt` (one
/// clock read, not two), `DeleteAt == 0`, and a **lower-cased** username — the submitted one has
/// capitals, so a port that skipped `NormalizeUsername` would store them and every later login by
/// username would miss.
#[tokio::test]
async fn bot_store_save_writes_both_rows_and_stamps_the_bot() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant_user(&pool, WRITE_OWNER, "mmrsbotownerwrite", "Write Owner", 0).await;

    let users = mm_store::user_store::SqlUserStore::new(pool.clone());
    let bots = SqlBotStore::new(pool.clone());

    let before = mm_model::utils::get_millis();
    let saved = create_bot(
        &users,
        &bots,
        "MmrsBotStoreSaved",
        "Saved Bot",
        "saved here",
    )
    .await
    .expect("the bot is created");

    assert!(
        mm_model::utils::is_valid_id(&saved.user_id),
        "PreSave minted the user id: {saved:?}"
    );
    assert_eq!(
        saved.username, "mmrsbotstoresaved",
        "NormalizeUsername lowercases"
    );
    assert_eq!(saved.display_name, "Saved Bot");
    assert_eq!(saved.description, "saved here");
    assert_eq!(saved.owner_id, WRITE_OWNER);
    assert_eq!(saved.delete_at, 0, "PreSave clears DeleteAt");
    assert_eq!(
        saved.create_at, saved.update_at,
        "PreSave reads the clock once and assigns both"
    );
    assert!(saved.create_at >= before);

    // What the read routes see: the same bot, through the join.
    let read = bots.get(&saved.user_id, false).await.expect("it is there");
    assert_eq!(read, saved, "the returned bot is the stored one");

    // And the user row behind it, which no `model.Bot` shows.
    let user = users.get(&saved.user_id).await.expect("the user row");
    assert_eq!(user.username, "mmrsbotstoresaved");
    assert_eq!(
        user.email, "mmrsbotstoresaved@localhost",
        "UserFromBot generates the address"
    );
    assert_eq!(user.first_name, "Saved Bot", "display_name is FirstName");
    assert_eq!(user.roles, "system_user");
    assert!(user.is_bot, "the join makes it a bot");

    purge(&pool).await;
}

/// **`PreSave` runs before `IsValid`, and reversing them fails every create.**
///
/// `IsValid` refuses a zero `CreateAt`, which is exactly what an unsaved bot has — so validating
/// first would reject the valid bot above. The order is only observable through a bot that is
/// invalid for some *other* reason: this one's username is not a username, and the error must be
/// `username`, not `create_at`.
#[tokio::test]
async fn bot_store_save_validates_after_stamping() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let bots = SqlBotStore::new(pool.clone());
    let bot = mm_model::bot::Bot {
        user_id: "mmrsbotstore00000000invld1".to_owned(),
        username: "no spaces allowed".to_owned(),
        owner_id: WRITE_OWNER.to_owned(),
        ..Default::default()
    };

    match bots.save(&bot).await {
        Err(mm_store::StoreError::Invalid { app_error, .. }) => {
            assert_eq!(app_error.id, "model.bot.is_valid.username.app_error");
            assert_eq!(app_error.status_code, 400);
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }

    assert!(
        bots.get("mmrsbotstore00000000invld1", true).await.is_err(),
        "a refused save writes no row"
    );
    purge(&pool).await;
}

/// **`Update` answers with the row it re-read, not with the bot it was handed.**
///
/// Five fields are copied onto the stored row and the rest of that row is kept, so a caller
/// passing a wrong `Username`, `DisplayName` or `CreateAt` gets the stored values back. That is
/// what makes `App.PatchBot`'s ordering load-bearing — it writes `Users` *first* so this re-read
/// picks up the new name — and a port that returned the caller's bot would answer with the old
/// username while having stored the new one, undetectably until the next `GET`.
#[tokio::test]
async fn bot_store_update_answers_with_the_stored_join() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant_user(&pool, WRITE_OWNER, "mmrsbotownerwrite", "Write Owner", 0).await;

    let users = mm_store::user_store::SqlUserStore::new(pool.clone());
    let bots = SqlBotStore::new(pool.clone());
    let saved = create_bot(&users, &bots, "mmrsbotstoreupd", "Before", "before")
        .await
        .expect("created");

    let submitted = mm_model::bot::Bot {
        user_id: saved.user_id.clone(),
        // Lies the store must ignore, every one of them a field it does not copy.
        username: "notthisname".to_owned(),
        display_name: "Not This Either".to_owned(),
        create_at: 1_i64,
        update_at: 1_i64,
        // Lies it must honour.
        description: "after".to_owned(),
        owner_id: "mmrsbotowner0000000000live".to_owned(),
        last_icon_update: 4242,
        delete_at: 0,
    };

    let before = mm_model::utils::get_millis();
    let updated = bots.update(&submitted).await.expect("the update lands");

    assert_eq!(
        updated.username, "mmrsbotstoreupd",
        "from the join, not the caller"
    );
    assert_eq!(updated.display_name, "Before", "from Users.FirstName");
    assert_eq!(updated.create_at, saved.create_at, "CreateAt is not copied");
    assert_eq!(updated.description, "after");
    assert_eq!(updated.owner_id, "mmrsbotowner0000000000live");
    assert_eq!(updated.last_icon_update, 4242);
    assert!(
        updated.update_at >= before,
        "PreUpdate stamps it, overriding the caller's 1"
    );

    assert_eq!(
        bots.get(&saved.user_id, true).await.expect("still there"),
        updated,
        "what was written is what was answered"
    );
    purge(&pool).await;
}

/// The soft-delete flip both activity routes drive, and the `Get` that has to see through it.
#[tokio::test]
async fn bot_store_update_flips_delete_at_in_both_directions() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant_user(&pool, WRITE_OWNER, "mmrsbotownerwrite", "Write Owner", 0).await;

    let users = mm_store::user_store::SqlUserStore::new(pool.clone());
    let bots = SqlBotStore::new(pool.clone());
    let saved = create_bot(&users, &bots, "mmrsbotstoreflip", "Flip", "flip")
        .await
        .expect("created");

    let mut off = saved.clone();
    off.delete_at = 1788600002000;
    let off = bots.update(&off).await.expect("disabled");
    assert_eq!(off.delete_at, 1788600002000);
    assert!(
        bots.get(&saved.user_id, false).await.is_err(),
        "a disabled bot is invisible without include_deleted"
    );
    // **`Update` re-reads with `includeDeleted = true`**, which is the only reason the next call
    // can find the row it is about to re-enable.
    let mut on = off.clone();
    on.delete_at = 0;
    let on = bots.update(&on).await.expect("re-enabled");
    assert_eq!(on.delete_at, 0);
    assert!(bots.get(&saved.user_id, false).await.is_ok());

    purge(&pool).await;
}

/// A write to a bot that is not there is `NotFound` — which the app layer turns back into the
/// same 404 a read gets, so a bot deleted between the permission check and the write is reported
/// as one that never existed.
#[tokio::test]
async fn bot_store_update_reports_a_miss() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let bots = SqlBotStore::new(pool.clone());
    let bot = mm_model::bot::Bot {
        user_id: "mmrsbotstore00000000gone01".to_owned(),
        username: "mmrsbotgone".to_owned(),
        owner_id: WRITE_OWNER.to_owned(),
        create_at: 1788600000000,
        update_at: 1788600000000,
        ..Default::default()
    };
    let err = bots.update(&bot).await.expect_err("no such bot");
    assert!(err.is_not_found(), "{err:?}");
    purge(&pool).await;
}

/// `User().Save`'s three refusals, each of which `App.CreateBot` turns into a different error id.
///
/// The **field** is what it branches on, so an email clash and a username clash have to be
/// distinguishable here or the client is told the wrong thing was taken.
#[tokio::test]
async fn bot_store_user_save_names_the_field_it_refused() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant_user(&pool, WRITE_OWNER, "mmrsbotownerwrite", "Write Owner", 0).await;

    let users = mm_store::user_store::SqlUserStore::new(pool.clone());
    let bots = SqlBotStore::new(pool.clone());
    create_bot(&users, &bots, "mmrsbotstoretaken", "Taken", "taken")
        .await
        .expect("the first one lands");

    // The same username. **Both** constraints are violated — `UserFromBot` derives the email
    // from the username — and the answer is `username`, not `email`, because the *database*
    // decides: Postgres reports one constraint per error and its unique index on `Username` is
    // the one it checked. Go reaches the same answer by a different route,
    // `strings.Contains(err.Error(), "users_username_key")` over the pq error text, so the order
    // of its two `IsUniqueConstraintError` calls is not what picks the field. Measured, after
    // this test asserted `email` on that reading and failed.
    match create_bot(&users, &bots, "mmrsbotstoretaken", "Again", "again").await {
        Err(mm_store::StoreError::InvalidInput { field, .. }) => assert_eq!(
            field, "username",
            "the constraint Postgres names is the one that decides"
        ),
        other => panic!("expected a unique-constraint refusal, got {other:?}"),
    }

    // The email branch, which `App.CreateBot` maps to a different error id and which the case
    // above cannot reach: a **non-bot** user already holding the address `UserFromBot` would
    // generate, under a username that is free.
    plant_user(
        &pool,
        "mmrsbotstore00000000mailer",
        "mmrsbotstoremailer",
        "Mailer",
        0,
    )
    .await;
    sqlx::query("UPDATE users SET email = 'mmrsbotstoreclash@localhost' WHERE id = $1")
        .bind("mmrsbotstore00000000mailer")
        .execute(&pool)
        .await
        .expect("the address is taken");
    match create_bot(&users, &bots, "mmrsbotstoreclash", "Clash", "clash").await {
        Err(mm_store::StoreError::InvalidInput { field, .. }) => assert_eq!(field, "email"),
        other => panic!("expected the email clash, got {other:?}"),
    }

    // A non-empty id on a non-remote user is refused before anything is written.
    let mut user = mm_model::user::User {
        id: "mmrsbotstore00000000hasid1".to_owned(),
        username: "mmrsbothasid".to_owned(),
        email: "mmrsbothasid@localhost".to_owned(),
        ..Default::default()
    };
    user.roles = "system_user".to_owned();
    match users.save(&user, &NoHasher).await {
        Err(mm_store::StoreError::InvalidInput { field, value, .. }) => {
            assert_eq!(field, "id");
            assert_eq!(value, "mmrsbotstore00000000hasid1");
        }
        other => panic!("expected the id refusal, got {other:?}"),
    }
    assert!(users.get("mmrsbotstore00000000hasid1").await.is_err());

    purge(&pool).await;
}

/// `App.CreateBot`'s rollback: the `Users` row it wrote before the `Bots` insert failed.
///
/// **A miss is not an error.** Go checks no `RowsAffected`, so the second delete here succeeds —
/// which matters because the app layer only logs this call's failure, and a store that errored
/// on a double delete would fill the log with a failure that is not one.
#[tokio::test]
async fn bot_store_permanent_delete_undoes_a_create_and_tolerates_a_miss() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    plant_user(&pool, WRITE_OWNER, "mmrsbotownerwrite", "Write Owner", 0).await;

    let users = mm_store::user_store::SqlUserStore::new(pool.clone());
    let saved = users
        .save(
            &mm_model::bot::user_from_bot(&mm_model::bot::Bot {
                username: "mmrsbotstorerollback".to_owned(),
                owner_id: WRITE_OWNER.to_owned(),
                ..Default::default()
            }),
            &NoHasher,
        )
        .await
        .expect("the user row lands");

    assert!(users.get(&saved.id).await.is_ok());
    users
        .permanent_delete(&saved.id)
        .await
        .expect("the rollback succeeds");
    assert!(users.get(&saved.id).await.is_err(), "the row is gone");
    users
        .permanent_delete(&saved.id)
        .await
        .expect("a second delete is not an error");

    purge(&pool).await;
}
