//! The four store methods behind the per-thread read-state routes, against a real Postgres:
//! `SqlThreadStore::mark_as_read`, `get_thread_unread_reply_count`, `update_membership` and
//! `SqlPostStore::get_posts_by_thread`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_thread_read_state
//! ```
//!
//! # Why the boundaries are here and not only in the parity suite
//!
//! The parity suite compares whole responses on real threads, where `create_at` values are
//! whatever the clock gave. The three comparisons that decide the counts — `>` against
//! `LastViewed` for unread replies, `>=` against `since` for the mention scan, and the
//! `DeleteAt = 0` on both — are asserted here on rows planted **at** the boundary, which the
//! parity suite can reach only by luck. `mark_as_read`'s "the argument, not the clock" and
//! `update_membership`'s "every column verbatim, including `LastUpdated`" are likewise
//! invisible from a body.
//!
//! Every row is `mmrstrs`-prefixed and purged before and after.

use mm_model::thread::ThreadMembership;
use mm_store::{PostStore, SqlPostStore, SqlThreadStore, ThreadStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrstrsuser000000000000001";
const AUTHOR: &str = "mmrstrsuser000000000000002";
const CHANNEL: &str = "mmrstrschan000000000000001";
const ROOT: &str = "mmrstrspost0000000000root1";
const OTHER_ROOT: &str = "mmrstrspost0000000000root2";
/// Replies to `ROOT`, by `create_at`.
const REPLY_AT_5: &str = "mmrstrspost00000000reply05";
const REPLY_AT_10: &str = "mmrstrspost00000000reply10";
const REPLY_AT_11: &str = "mmrstrspost00000000reply11";
const REPLY_AT_20: &str = "mmrstrspost00000000reply20";
/// At 15, but deleted.
const REPLY_DELETED: &str = "mmrstrspost0000000replydel";
/// A reply to the other thread at 30.
const OTHER_REPLY: &str = "mmrstrspost000000000other1";

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
        "DELETE FROM threadmemberships WHERE postid LIKE 'mmrstrs%' OR userid LIKE 'mmrstrs%'",
        "DELETE FROM threads WHERE postid LIKE 'mmrstrs%'",
        "DELETE FROM posts WHERE id LIKE 'mmrstrs%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn insert_post(pool: &PgPool, id: &str, root_id: &str, create_at: i64, delete_at: i64) {
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid,
                            channelid, rootid, originalid, message, type, props, hashtags,
                            filenames, fileids, hasreactions, remoteid)
         VALUES ($1, $2, $2, 0, $3, false, $4, $5, $6, '', 'mmrs read state', '', '{}', '',
                 '[]', '[]', false, NULL)",
    )
    .bind(id)
    .bind(create_at)
    .bind(delete_at)
    .bind(AUTHOR)
    .bind(CHANNEL)
    .bind(root_id)
    .execute(pool)
    .await
    .expect("inserts the post");
}

async fn seed(pool: &PgPool) {
    insert_post(pool, ROOT, "", 1, 0).await;
    insert_post(pool, OTHER_ROOT, "", 2, 0).await;
    insert_post(pool, REPLY_AT_5, ROOT, 5, 0).await;
    insert_post(pool, REPLY_AT_10, ROOT, 10, 0).await;
    insert_post(pool, REPLY_AT_11, ROOT, 11, 0).await;
    insert_post(pool, REPLY_DELETED, ROOT, 15, 99).await;
    insert_post(pool, REPLY_AT_20, ROOT, 20, 0).await;
    insert_post(pool, OTHER_REPLY, OTHER_ROOT, 30, 0).await;

    for (root, last_reply_at, reply_count) in [(ROOT, 20_i64, 4_i64), (OTHER_ROOT, 30, 1)] {
        sqlx::query(
            "INSERT INTO threads (postid, replycount, lastreplyat, participants, channelid,
                                  threaddeleteat, threadteamid)
             VALUES ($1, $2, $3, '[]'::jsonb, $4, 0, '')",
        )
        .bind(root)
        .bind(reply_count)
        .bind(last_reply_at)
        .bind(CHANNEL)
        .execute(pool)
        .await
        .expect("inserts the thread");
    }

    sqlx::query(
        "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated,
                                        unreadmentions)
         VALUES ($1, $2, true, 10, 42, 7)",
    )
    .bind(ROOT)
    .bind(USER)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

async fn membership(pool: &PgPool, post: &str) -> (bool, i64, i64, i64) {
    sqlx::query_as(
        "SELECT following, lastviewed, lastupdated, unreadmentions FROM threadmemberships
          WHERE postid = $1 AND userid = $2",
    )
    .bind(post)
    .bind(USER)
    .fetch_one(pool)
    .await
    .expect("the membership is there")
}

fn planted_membership(last_viewed: i64) -> ThreadMembership {
    ThreadMembership {
        post_id: ROOT.to_owned(),
        user_id: USER.to_owned(),
        following: true,
        last_updated: 42,
        last_viewed,
        unread_mentions: 7,
    }
}

/// `LastViewed` becomes the argument — backwards is fine — `LastUpdated` becomes the clock,
/// and `UnreadMentions` is not touched. A row that does not exist is a no-op.
#[tokio::test]
async fn store_mark_as_read_writes_the_timestamp_and_the_clock() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlThreadStore::new(pool.clone());
    let before = mm_model::utils::get_millis();

    store.mark_as_read(USER, ROOT, 3).await.expect("marks");
    let (following, last_viewed, last_updated, unread_mentions) = membership(&pool, ROOT).await;
    assert!(following);
    assert_eq!(
        last_viewed, 3,
        "the argument, even though it moves the mark backwards"
    );
    assert!(
        last_updated >= before,
        "the clock, not the argument: {last_updated}"
    );
    assert_eq!(unread_mentions, 7, "not this statement's column");

    store
        .mark_as_read(USER, OTHER_ROOT, 3)
        .await
        .expect("a missing row is not an error");
    store
        .mark_as_read("mmrstrsnobody00000000000000", ROOT, 3)
        .await
        .expect("nor is a missing user");

    purge(&pool).await;
}

/// Undeleted replies to the root created **strictly after** `LastViewed`: the reply at exactly
/// `LastViewed` is read, the deleted one and the other thread's do not exist, the root is not a
/// reply.
#[tokio::test]
async fn store_unread_reply_count_is_strict_and_skips_deleted() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlThreadStore::new(pool.clone());

    let count = |last_viewed: i64| {
        let store = store.clone();
        async move {
            store
                .get_thread_unread_reply_count(&planted_membership(last_viewed))
                .await
                .expect("counts")
        }
    };
    assert_eq!(count(0).await, 4, "5, 10, 11, 20; not the deleted 15");
    assert_eq!(
        count(10).await,
        2,
        "11 and 20: the reply at exactly 10 is read"
    );
    assert_eq!(count(11).await, 1);
    assert_eq!(count(20).await, 0);
    assert_eq!(count(-1).await, 4, "the root at 1 is not a reply");

    let other = store
        .get_thread_unread_reply_count(&ThreadMembership {
            post_id: OTHER_ROOT.to_owned(),
            ..planted_membership(0)
        })
        .await
        .expect("counts");
    assert_eq!(
        other, 1,
        "the other thread's own reply, and none of this one's"
    );

    purge(&pool).await;
}

/// Every one of the four columns is written from the struct, `LastUpdated` included — no clock,
/// no guard. A row that does not exist is a no-op, not an insert.
#[tokio::test]
async fn store_update_membership_writes_all_four_columns_verbatim() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlThreadStore::new(pool.clone());

    store
        .update_membership(&ThreadMembership {
            post_id: ROOT.to_owned(),
            user_id: USER.to_owned(),
            following: false,
            last_updated: 1234,
            last_viewed: 5678,
            unread_mentions: 9,
        })
        .await
        .expect("updates");
    assert_eq!(membership(&pool, ROOT).await, (false, 5678, 1234, 9));

    store
        .update_membership(&ThreadMembership {
            post_id: OTHER_ROOT.to_owned(),
            ..planted_membership(0)
        })
        .await
        .expect("a missing row is not an error");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threadmemberships WHERE userid = $1")
        .bind(USER)
        .fetch_one(&pool)
        .await
        .expect("counts");
    assert_eq!(rows, 1, "and it did not insert one");

    purge(&pool).await;
}

/// Replies to the thread with `CreateAt >= since` — **inclusive**, unlike the unread count —
/// and `DeleteAt = 0`; never the root, never another thread's.
#[tokio::test]
async fn store_posts_by_thread_is_inclusive_and_skips_deleted() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    let ids = |since: i64| {
        let store = store.clone();
        async move {
            let mut posts = store
                .get_posts_by_thread(ROOT, since)
                .await
                .expect("fetches");
            posts.sort_by_key(|p| p.create_at);
            posts.into_iter().map(|p| p.id).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        ids(0).await,
        vec![REPLY_AT_5, REPLY_AT_10, REPLY_AT_11, REPLY_AT_20],
        "all four live replies, not the root, not the deleted one"
    );
    assert_eq!(
        ids(10).await,
        vec![REPLY_AT_10, REPLY_AT_11, REPLY_AT_20],
        "at exactly `since` is included"
    );
    assert_eq!(ids(21).await, Vec::<String>::new());

    let posts = store.get_posts_by_thread(ROOT, 5).await.expect("fetches");
    let first = posts
        .iter()
        .find(|p| p.id == REPLY_AT_5)
        .expect("the reply at 5");
    assert_eq!(first.root_id, ROOT);
    assert_eq!(first.user_id, AUTHOR);
    assert_eq!(first.message, "mmrs read state");
    assert_eq!(
        first.reply_count, 4,
        "the shared select carries the thread's live reply count"
    );

    purge(&pool).await;
}
