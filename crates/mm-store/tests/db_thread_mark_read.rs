//! `SqlThreadStore::mark_all_as_read_by_channels` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_thread_mark_read
//! ```
//!
//! # Why it is here and not only in the parity suite
//!
//! The statement is reached by three routes and **no response body shows what it did**: the
//! channel-view family answers with `last_viewed_at_times`, which is computed from
//! `ChannelMembers` before this runs. Over HTTP the only visible consequence is a
//! `thread_read_changed` event, which is published on a *decision* about this statement rather
//! than on its result — so an implementation that published the event and wrote nothing would
//! pass every cross-server test.
//!
//! Two predicates are the whole behaviour and both are asserted here:
//!
//! - **`Threads.LastReplyAt > ThreadMemberships.LastViewed`** — a membership already caught up is
//!   not touched, so its `LastUpdated` does not move. Without it, "mark a whole team read" would
//!   rewrite every thread membership the user has in that team on every press.
//! - **`Threads.ChannelId = ANY(...)`** — the scope. A thread in a channel outside the list is
//!   left alone even though the membership belongs to the same user.
//!
//! Every row here is `mmrsthr`-prefixed and purged before and after.

use mm_store::{SqlThreadStore, ThreadStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsthruser00000000000user";
const IN_SCOPE: &str = "mmrsthrchan000000000scope";
const OUT_OF_SCOPE: &str = "mmrsthrchan00000000outsid";
/// Stale: the thread has replies the member has not seen.
const STALE: &str = "mmrsthrpost0000000000stale";
/// Caught up: `LastViewed` is already past `LastReplyAt`.
const CAUGHT_UP: &str = "mmrsthrpost000000000caught";
/// Stale, but in a channel the caller does not pass.
const ELSEWHERE: &str = "mmrsthrpost00000000elsewhr";

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
        "DELETE FROM threadmemberships WHERE postid LIKE 'mmrsthr%' OR userid LIKE 'mmrsthr%'",
        "DELETE FROM threads WHERE postid LIKE 'mmrsthr%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Three threads and three memberships, differing only in the two columns the predicates read.
async fn seed(pool: &PgPool) {
    for (post, channel, last_reply_at) in [
        (STALE, IN_SCOPE, 5_000_i64),
        (CAUGHT_UP, IN_SCOPE, 5_000),
        (ELSEWHERE, OUT_OF_SCOPE, 5_000),
    ] {
        sqlx::query(
            "INSERT INTO threads (postid, replycount, lastreplyat, participants, channelid,
                                  threaddeleteat, threadteamid)
             VALUES ($1, 2, $2, '[]'::jsonb, $3, 0, '')",
        )
        .bind(post)
        .bind(last_reply_at)
        .bind(channel)
        .execute(pool)
        .await
        .expect("inserts the thread");
    }

    // `LastViewed`: below `LastReplyAt` for two of them, above it for the third.
    for (post, last_viewed) in [(STALE, 1_000_i64), (CAUGHT_UP, 9_000), (ELSEWHERE, 1_000)] {
        sqlx::query(
            "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated,
                                            unreadmentions)
             VALUES ($1, $2, true, $3, 42, 7)",
        )
        .bind(post)
        .bind(USER)
        .bind(last_viewed)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
}

type Membership = (i64, i64, i64);

async fn membership(pool: &PgPool, post: &str) -> Membership {
    sqlx::query_as(
        "SELECT lastviewed, lastupdated, unreadmentions FROM threadmemberships
          WHERE postid = $1 AND userid = $2",
    )
    .bind(post)
    .bind(USER)
    .fetch_one(pool)
    .await
    .expect("the membership is there")
}

#[tokio::test]
async fn store_thread_mark_read_moves_only_the_stale_membership_in_scope() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let before = mm_model::utils::get_millis();
    SqlThreadStore::new(pool.clone())
        .mark_all_as_read_by_channels(USER, &[IN_SCOPE.to_owned()])
        .await
        .expect("the update runs");

    let stale = membership(&pool, STALE).await;
    let caught_up = membership(&pool, CAUGHT_UP).await;
    let elsewhere = membership(&pool, ELSEWHERE).await;

    purge(&pool).await;

    assert!(
        stale.0 >= before,
        "the stale membership's LastViewed is now the clock: {stale:?}"
    );
    assert_eq!(
        stale.0, stale.1,
        "LastViewed and LastUpdated come from one GetMillis() call"
    );
    assert_eq!(stale.2, 0, "and its unread mentions are cleared");

    assert_eq!(
        caught_up,
        (9_000, 42, 7),
        "LastReplyAt > LastViewed is false, so nothing about this row moved"
    );
    assert_eq!(
        elsewhere,
        (1_000, 42, 7),
        "a thread outside the channel list is untouched even though it is stale"
    );
}

/// An empty channel list sends no statement at all — Go returns before it builds one.
#[tokio::test]
async fn store_thread_mark_read_an_empty_channel_list_is_a_no_op() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    SqlThreadStore::new(pool.clone())
        .mark_all_as_read_by_channels(USER, &[])
        .await
        .expect("an empty list is not an error");

    let stale = membership(&pool, STALE).await;
    purge(&pool).await;

    assert_eq!(stale, (1_000, 42, 7), "nothing was written");
}
