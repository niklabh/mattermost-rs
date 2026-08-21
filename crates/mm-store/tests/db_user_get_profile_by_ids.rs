//! `SqlUserStore::get_profile_by_ids` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_get_profile_by_ids
//! ```
//!
//! The branches of `GetProfileByIds` (user_store.go:1172) that only a database can show: the
//! `Since > 0` guard (zero and negative are *no filter*, and a row with `UpdateAt = 0` proves
//! it), no `DeleteAt` predicate (a deactivated user is returned), an unknown id is absent rather
//! than an error, `ORDER BY Username`, and the `Bots` join that `is_bot` depends on.

use mm_store::{UserStore, user_store::SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Ids are minted so that **id order and username order disagree**: `usr1` is `zed`, `usr2`
/// is `amy`, `usr3` is `mid` — a query that forgot `ORDER BY Username` and came back in
/// primary-key order would be caught.
const ZED: &str = "mmrsbyids00000000000000us1";
const AMY: &str = "mmrsbyids00000000000000us2";
const MID: &str = "mmrsbyids00000000000000us3";
const GONE: &str = "mmrsbyids00000000000000us4";
const STALE: &str = "mmrsbyids00000000000000us5";
const BOT: &str = "mmrsbyids00000000000000us6";
/// `UpdateAt = -5`: no real row has one, but without it `Since > 0` and `Since != 0` answer
/// every request identically — a mutation that survived until this row existed.
const NEG: &str = "mmrsbyids00000000000000us7";
const UNKNOWN: &str = "mmrsbyids0000000000000none";

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
    for statement in [
        "DELETE FROM bots WHERE userid LIKE 'mmrsbyids%'",
        "DELETE FROM users WHERE id LIKE 'mmrsbyids%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn insert_user(pool: &PgPool, id: &str, username: &str, update_at: i64, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, email, roles, lastlogin)
         VALUES ($1, 1000, $2, $3, $4, $1 || '@mmrs.invalid', 'system_user', 0)",
    )
    .bind(id)
    .bind(update_at)
    .bind(delete_at)
    .bind(username)
    .execute(pool)
    .await
    .expect("inserts the user");
}

async fn seed(pool: &PgPool) {
    purge(pool).await;
    insert_user(pool, ZED, "mmrsbyids-zed", 3_000, 0).await;
    insert_user(pool, AMY, "mmrsbyids-amy", 1_000, 0).await;
    insert_user(pool, MID, "mmrsbyids-mid", 2_000, 0).await;
    insert_user(pool, GONE, "mmrsbyids-gone", 2_500, 9_999).await;
    // `UpdateAt = 0`: the row that separates "Since is zero, no filter" from "UpdateAt > 0".
    insert_user(pool, STALE, "mmrsbyids-stale", 0, 0).await;
    insert_user(pool, BOT, "mmrsbyids-bot", 2_000, 0).await;
    insert_user(pool, NEG, "mmrsbyids-neg", -5, 0).await;
    sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat, lasticonupdate)
         VALUES ($1, 'a fixture bot', $2, 1000, 1000, 0, 4242)",
    )
    .bind(BOT)
    .bind(ZED)
    .execute(pool)
    .await
    .expect("inserts the bot row");
}

fn ids(users: &[mm_model::user::User]) -> Vec<&str> {
    users.iter().map(|u| u.id.as_str()).collect()
}

fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn order_is_username_deleted_rows_are_kept_and_unknown_ids_are_absent() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    // Asked for in id order, with an unknown id in the middle.
    let users = store
        .get_profile_by_ids(&owned(&[ZED, AMY, UNKNOWN, GONE, MID, STALE, BOT, NEG]), 0)
        .await
        .expect("query runs");

    assert_eq!(
        ids(&users),
        vec![AMY, BOT, GONE, MID, NEG, STALE, ZED],
        "Username ASC — not id order, not request order; the deleted user and the zero-UpdateAt \
         user are both present; the unknown id is simply missing"
    );

    let gone = users
        .iter()
        .find(|u| u.id == GONE)
        .expect("deactivated user is returned");
    assert_eq!(gone.delete_at, 9_999, "no DeleteAt filter");

    let bot = users.iter().find(|u| u.id == BOT).expect("present");
    assert!(bot.is_bot, "the Bots join is what makes is_bot true");
    assert_eq!(bot.bot_description, "a fixture bot");
    assert_eq!(bot.bot_last_icon_update, 4242);
    let human = users.iter().find(|u| u.id == AMY).expect("present");
    assert!(!human.is_bot);
    assert_eq!(
        human.bot_last_icon_update, 0,
        "COALESCE over the missing join"
    );

    purge(&pool).await;
}

#[tokio::test]
async fn since_filters_only_when_positive_and_is_strictly_greater() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());
    let all = owned(&[ZED, AMY, MID, GONE, STALE, BOT, NEG]);

    // `Since > 0` is the guard: zero and a negative value apply no predicate at all, so the
    // `UpdateAt = 0` row survives both — and the `UpdateAt = -5` row survives `since=-1`,
    // which a `!= 0` guard (`UpdateAt > -1`) would drop.
    for no_filter in [0, -1] {
        let users = store
            .get_profile_by_ids(&all, no_filter)
            .await
            .expect("query runs");
        assert_eq!(users.len(), 7, "since={no_filter} is no filter");
        assert!(ids(&users).contains(&STALE), "since={no_filter}");
        assert!(ids(&users).contains(&NEG), "since={no_filter}");
    }

    // Strictly greater: 2000 excludes the two rows *at* 2000 and everything below.
    let users = store
        .get_profile_by_ids(&all, 2_000)
        .await
        .expect("query runs");
    assert_eq!(
        ids(&users),
        vec![GONE, ZED],
        "UpdateAt > 2000 — 2500 (deleted, still in) and 3000; MID and BOT sit exactly on the \
         boundary and are out"
    );

    // 1 is positive, so the filter applies and drops the zero- and negative-UpdateAt rows.
    let users = store.get_profile_by_ids(&all, 1).await.expect("query runs");
    assert_eq!(users.len(), 5);
    assert!(!ids(&users).contains(&STALE), "since=1 is a real filter");
    assert!(!ids(&users).contains(&NEG));

    // An empty id list is an empty answer, not an error.
    let users = store.get_profile_by_ids(&[], 0).await.expect("query runs");
    assert!(users.is_empty());

    purge(&pool).await;
}
