//! `SqlPostStore::save` for a **reply**: the `Threads` bookkeeping Go does inside the insert's
//! transaction (`updateThreadsFromPosts`, post_store.go:270), the root's `UpdateAt` bump, the
//! reply-count read-back, and the channel counters that move differently for a reply.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_post_save_reply
//! ```
//!
//! # Why here and not only in the parity suite
//!
//! The parity suite compares what `GET …/threads/{id}` shows, which is the `Threads` row read
//! back through a join that coalesces and reorders nothing it does not select. The
//! participants' **order** (latest reply last), the `ThreadTeamId`, the root's `UpdateAt` and the
//! two `Root` channel counters that must *not* move are all only visible to a direct read.
//!
//! Every row is `mmrsrply`-prefixed and purged before and after.

use mm_model::post::Post;
use mm_store::{PostStore, SqlPostStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsrplyteam00000000000001";
const CHANNEL: &str = "mmrsrplychan00000000000001";
const ALICE: &str = "mmrsrplyuser0000000000alic";
const BOB: &str = "mmrsrplyuser00000000000bob";
const ROOT: &str = "mmrsrplypost000000000root1";
const T_ROOT: i64 = 1_600_000_000_000;

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
        "DELETE FROM threads WHERE postid LIKE 'mmrsrply%' OR channelid LIKE 'mmrsrply%'",
        "DELETE FROM posts WHERE id LIKE 'mmrsrply%' OR channelid LIKE 'mmrsrply%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsrply%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn seed(pool: &PgPool) {
    purge(pool).await;
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                               lastpostat, lastrootpostat, totalmsgcount, totalmsgcountroot)
         VALUES ($1, 0, 0, 0, $2, 'O'::channel_type, 'mmrs reply', $1, $3, $3, 1, 1)",
    )
    .bind(CHANNEL)
    .bind(TEAM)
    .bind(T_ROOT)
    .execute(pool)
    .await
    .expect("inserts the channel");
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid, channelid,
                            rootid, originalid, message, type, props, hashtags, filenames, fileids,
                            hasreactions, remoteid)
         VALUES ($1, $2, $2, 0, 0, false, $3, $4, '', '', 'root', '', '{}', '', '[]', '[]', false, NULL)",
    )
    .bind(ROOT)
    .bind(T_ROOT)
    .bind(ALICE)
    .bind(CHANNEL)
    .execute(pool)
    .await
    .expect("inserts the root");
}

fn reply(user_id: &str, create_at: i64) -> Post {
    Post {
        user_id: user_id.to_owned(),
        channel_id: CHANNEL.to_owned(),
        root_id: ROOT.to_owned(),
        message: "a reply".to_owned(),
        create_at,
        ..Default::default()
    }
}

#[derive(Debug, PartialEq)]
struct ThreadRow {
    channel_id: String,
    reply_count: i64,
    last_reply_at: i64,
    participants: Vec<String>,
    team_id: String,
    delete_at: Option<i64>,
}

async fn thread_row(pool: &PgPool) -> Option<ThreadRow> {
    let row: Option<(String, i64, i64, serde_json::Value, String, Option<i64>)> = sqlx::query_as(
        "SELECT channelid, replycount, lastreplyat, participants, threadteamid, threaddeleteat
           FROM threads WHERE postid = $1",
    )
    .bind(ROOT)
    .fetch_optional(pool)
    .await
    .expect("reads the thread row");
    row.map(
        |(channel_id, reply_count, last_reply_at, participants, team_id, delete_at)| ThreadRow {
            channel_id,
            reply_count,
            last_reply_at,
            participants: serde_json::from_value(participants).expect("a string array"),
            team_id,
            delete_at,
        },
    )
}

async fn channel_counters(pool: &PgPool) -> (i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT lastpostat, lastrootpostat, totalmsgcount, totalmsgcountroot FROM channels WHERE id = $1",
    )
    .bind(CHANNEL)
    .fetch_one(pool)
    .await
    .expect("reads the channel")
}

async fn root_update_at(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT updateat FROM posts WHERE id = $1")
        .bind(ROOT)
        .fetch_one(pool)
        .await
        .expect("reads the root")
}

/// The first reply creates the `Threads` row from the table; later replies advance it, moving
/// a repeat author to the end of the participants.
#[tokio::test]
async fn store_save_reply_creates_then_advances_the_thread_row() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    assert_eq!(
        thread_row(&pool).await,
        None,
        "no row before the first reply"
    );

    let first = store.save(&reply(ALICE, T_ROOT + 10)).await.expect("saves");
    assert_eq!(
        first.reply_count, 1,
        "populateReplyCount counts the reply itself"
    );
    assert_eq!(
        thread_row(&pool).await,
        Some(ThreadRow {
            channel_id: CHANNEL.to_owned(),
            reply_count: 1,
            last_reply_at: T_ROOT + 10,
            participants: vec![ALICE.to_owned()],
            team_id: TEAM.to_owned(),
            delete_at: None,
        }),
        "built from the table: the reply is in the same transaction"
    );

    let second = store.save(&reply(BOB, T_ROOT + 20)).await.expect("saves");
    assert_eq!(second.reply_count, 2);
    let row = thread_row(&pool).await.expect("the row");
    assert_eq!(row.reply_count, 2);
    assert_eq!(row.last_reply_at, T_ROOT + 20);
    assert_eq!(row.participants, vec![ALICE.to_owned(), BOB.to_owned()]);

    // Alice again: removed from where she was and appended at the end.
    let third = store.save(&reply(ALICE, T_ROOT + 30)).await.expect("saves");
    assert_eq!(third.reply_count, 3);
    let row = thread_row(&pool).await.expect("the row");
    assert_eq!(row.participants, vec![BOB.to_owned(), ALICE.to_owned()]);
    assert_eq!(row.last_reply_at, T_ROOT + 30);

    // An older reply arriving late raises the count but not `LastReplyAt`.
    let _ = store.save(&reply(BOB, T_ROOT + 25)).await.expect("saves");
    let row = thread_row(&pool).await.expect("the row");
    assert_eq!(row.reply_count, 4);
    assert_eq!(
        row.last_reply_at,
        T_ROOT + 30,
        "LastReplyAt only ever rises"
    );
    assert_eq!(row.participants, vec![ALICE.to_owned(), BOB.to_owned()]);

    purge(&pool).await;
}

/// A reply moves `LastPostAt` and `TotalMsgCount`, leaves the two `Root` columns alone, and sets
/// the root's `UpdateAt` to its own `CreateAt`.
#[tokio::test]
async fn store_save_reply_moves_the_non_root_counters_and_the_roots_update_at() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    assert_eq!(channel_counters(&pool).await, (T_ROOT, T_ROOT, 1, 1));
    assert_eq!(root_update_at(&pool).await, T_ROOT);

    store.save(&reply(BOB, T_ROOT + 50)).await.expect("saves");
    assert_eq!(
        channel_counters(&pool).await,
        (T_ROOT + 50, T_ROOT, 2, 1),
        "LastPostAt and TotalMsgCount move; LastRootPostAt and TotalMsgCountRoot do not"
    );
    assert_eq!(root_update_at(&pool).await, T_ROOT + 50);

    // A root beside it moves all four.
    let root = Post {
        user_id: BOB.to_owned(),
        channel_id: CHANNEL.to_owned(),
        message: "another root".to_owned(),
        create_at: T_ROOT + 60,
        ..Default::default()
    };
    let saved = store.save(&root).await.expect("saves");
    assert_eq!(saved.reply_count, 0);
    assert_eq!(
        channel_counters(&pool).await,
        (T_ROOT + 60, T_ROOT + 60, 3, 2)
    );

    purge(&pool).await;
}

/// The first reply's participants come from the table in latest-reply order, not from the
/// reply alone: a thread whose `Threads` row is missing (the fixture database held thousands)
/// is rebuilt from every live reply.
#[tokio::test]
async fn store_save_reply_rebuilds_participants_from_every_live_reply() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    // Two earlier replies with no Threads row — Bob first, then Alice — plus a deleted one by a
    // third author that must not count.
    for (id, user, at, delete_at) in [
        ("mmrsrplypost0000000000old1", BOB, T_ROOT + 1, 0_i64),
        ("mmrsrplypost0000000000old2", ALICE, T_ROOT + 2, 0),
        (
            "mmrsrplypost0000000000gone",
            "mmrsrplyuser000000000carol",
            T_ROOT + 3,
            99,
        ),
    ] {
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid, channelid,
                                rootid, originalid, message, type, props, hashtags, filenames, fileids,
                                hasreactions, remoteid)
             VALUES ($1, $2, $2, 0, $3, false, $4, $5, $6, '', 'old', '', '{}', '', '[]', '[]', false, NULL)",
        )
        .bind(id)
        .bind(at)
        .bind(delete_at)
        .bind(user)
        .bind(CHANNEL)
        .bind(ROOT)
        .execute(&pool)
        .await
        .expect("inserts");
    }

    let saved = store.save(&reply(BOB, T_ROOT + 40)).await.expect("saves");
    assert_eq!(
        saved.reply_count, 3,
        "two old live replies plus this one; not the deleted one"
    );
    let row = thread_row(&pool).await.expect("the row");
    assert_eq!(row.reply_count, 3);
    assert_eq!(row.last_reply_at, T_ROOT + 40);
    // Ordered by each author's latest reply: Alice's latest is +2, Bob's is +40.
    assert_eq!(row.participants, vec![ALICE.to_owned(), BOB.to_owned()]);

    purge(&pool).await;
}

/// Deleting a reply recomputes the thread's live counters and drops the author from the
/// participants only when it was their last live reply (`updateThreadAfterReplyDeletion`);
/// the root's `UpdateAt` moves to the delete time.
#[tokio::test]
async fn store_delete_reply_recomputes_the_thread_and_drops_a_last_participant() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    let a1 = store.save(&reply(ALICE, T_ROOT + 10)).await.expect("saves");
    let b1 = store.save(&reply(BOB, T_ROOT + 20)).await.expect("saves");
    let a2 = store.save(&reply(ALICE, T_ROOT + 30)).await.expect("saves");
    assert_eq!(
        thread_row(&pool).await.expect("row").participants,
        vec![BOB.to_owned(), ALICE.to_owned()]
    );

    // Alice's latest goes: she still has one live reply, so she stays a participant.
    store
        .delete(&a2.id, T_ROOT + 100, BOB)
        .await
        .expect("deletes");
    let row = thread_row(&pool).await.expect("row");
    assert_eq!(row.reply_count, 2);
    assert_eq!(
        row.last_reply_at,
        T_ROOT + 20,
        "recomputed from the live replies"
    );
    assert_eq!(row.participants, vec![BOB.to_owned(), ALICE.to_owned()]);
    assert_eq!(
        root_update_at(&pool).await,
        T_ROOT + 100,
        "the root's UpdateAt is the delete time"
    );

    // Bob's only reply goes: he is removed from the participants.
    store
        .delete(&b1.id, T_ROOT + 110, BOB)
        .await
        .expect("deletes");
    let row = thread_row(&pool).await.expect("row");
    assert_eq!(row.reply_count, 1);
    assert_eq!(row.last_reply_at, T_ROOT + 10);
    assert_eq!(row.participants, vec![ALICE.to_owned()]);

    // The last reply: count 0, last reply 0, nobody left. The `ReplyCount > 0` guard still
    // admits this row (it read 1).
    store
        .delete(&a1.id, T_ROOT + 120, BOB)
        .await
        .expect("deletes");
    let row = thread_row(&pool).await.expect("row");
    assert_eq!(row.reply_count, 0);
    assert_eq!(row.last_reply_at, 0);
    assert_eq!(row.participants, Vec::<String>::new());
    assert_eq!(
        row.delete_at, None,
        "a reply delete never marks the thread deleted"
    );

    purge(&pool).await;
}
