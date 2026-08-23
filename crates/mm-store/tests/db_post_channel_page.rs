//! The parts of `SqlPostStore`'s channel-page surface that `GET /api/v4/channels/{id}/posts`
//! cannot reach, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_post_channel_page
//! ```
//!
//! # Why this exists next to a cross-server parity suite that already covers the route
//!
//! Two queries behind that route are invisible over HTTP, for opposite reasons.
//!
//! - **`get_post_id_around_time`** is the cursor lookup Go uses when burn-on-read is *off*.
//!   `ServiceSettings.EnableBurnOnRead` and `FeatureFlags.BurnOnRead` both default to `true`, and
//!   both are true on the stack the parity suite runs against, so every cursor there comes from
//!   the read-receipt-aware sibling instead. Deleting this query outright passes that suite.
//! - **`get_visible_post_id_around_time`'s burn-on-read predicate** is the reverse: the query
//!   runs on every request, but the predicate only *does* anything for a burn-on-read post, and
//!   a list containing one is refused by the metadata pipeline and forwarded to Go. So the parity
//!   suite can never see the predicate work, and dropping it — which is to say, letting the
//!   cursor point at a post the caller can no longer see — passes there too.
//!
//! Both are exercised here against rows written directly, which is also the only way to get a
//! `ReadReceipts` row with an expiry in the past.
//!
//! # These are transcribed from the Go source, not measured against the Go server
//!
//! Unlike the parity suite, there is no oracle behind these assertions: `SqlPostStore` is not
//! reachable from a test and the route in front of it cannot express the input. They are read off
//! post_store.go:1822 and :1887 and asserted against our own implementation. If upstream changes
//! the predicate, this keeps passing while the port drifts.

use mm_store::post_store::{PostStore, SqlPostStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The tests share one set of rows and each purges before seeding, so two running interleaved
/// would delete each other's fixtures mid-assertion. Serialised here rather than by asking the
/// operator for `--test-threads=1`, which is the kind of instruction that gets lost.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CHANNEL: &str = "mmrspostpage00000000chan00";
const AUTHOR: &str = "mmrspostpage00000000user00";
const READER: &str = "mmrspostpage00000000user01";

/// Fixed `CreateAt` values, a thousand apart, so "the next post" is unambiguous and a strict
/// comparison has room on both sides.
const T_ROOT: i64 = 1_600_000_000_000;
const T_REPLY: i64 = T_ROOT + 1_000;
const T_BURN: i64 = T_ROOT + 2_000;
const T_DELETED: i64 = T_ROOT + 3_000;
const T_LAST: i64 = T_ROOT + 4_000;

const ROOT: &str = "mmrspostpage000000000root0";
const REPLY: &str = "mmrspostpage00000000reply0";
const BURN: &str = "mmrspostpage000000000burn0";
const DELETED: &str = "mmrspostpage000000000del00";
const LAST: &str = "mmrspostpage000000000last0";

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
        "DELETE FROM readreceipts WHERE postid LIKE 'mmrspostpage%' OR userid LIKE 'mmrspostpage%'",
        "DELETE FROM posts WHERE id LIKE 'mmrspostpage%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Five posts in one channel: a root, a reply to it, a burn-on-read post by `AUTHOR`, a
/// soft-deleted post, and a last root.
///
/// **Every column is written**, including the ones a shorter insert would leave NULL: Go's
/// `model.Post` scans them into non-pointer fields, and a NULL there makes the *Go* server's
/// `GET /api/v4/users`-style reads fail for every worktree sharing this database. That is the
/// failure [D-157] closed for `Users`; it is not repeated here for `Posts`.
async fn seed(pool: &PgPool) {
    purge(pool).await;

    for (id, create_at, root_id, post_type, delete_at, user_id) in [
        (ROOT, T_ROOT, "", "", 0, AUTHOR),
        (REPLY, T_REPLY, ROOT, "", 0, AUTHOR),
        (BURN, T_BURN, "", "burn_on_read", 0, AUTHOR),
        (DELETED, T_DELETED, "", "", T_DELETED, AUTHOR),
        (LAST, T_LAST, "", "", 0, AUTHOR),
    ] {
        sqlx::query(
            "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid,
                                channelid, rootid, originalid, message, type, props, hashtags,
                                filenames, fileids, hasreactions, remoteid)
             VALUES ($1, $2, $2, 0, $3, false, $4, $5, $6, '', 'x', $7, '{}', '', '[]', '[]',
                     false, NULL)",
        )
        .bind(id)
        .bind(create_at)
        .bind(delete_at)
        .bind(user_id)
        .bind(CHANNEL)
        .bind(root_id)
        .bind(post_type)
        .execute(pool)
        .await
        .expect("the fixture post inserts");
    }
}

async fn read_receipt(pool: &PgPool, post_id: &str, user_id: &str, expire_at: i64) {
    sqlx::query(
        "INSERT INTO readreceipts (postid, userid, expireat) VALUES ($1, $2, $3)
         ON CONFLICT (postid, userid) DO UPDATE SET expireat = EXCLUDED.expireat",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(expire_at)
    .execute(pool)
    .await
    .expect("the read receipt inserts");
}

/// The plain cursor: strictly newer, oldest first, deleted posts skipped.
#[tokio::test]
async fn the_plain_cursor_steps_one_post_in_each_direction() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_ROOT, false, false)
            .await
            .expect("the query runs"),
        REPLY,
        "after the root comes the reply"
    );
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_REPLY, true, false)
            .await
            .expect("the query runs"),
        ROOT,
        "and before the reply comes the root"
    );

    // The comparison is strict on both sides (`sq.Lt` / `sq.Gt`), so a post created in the very
    // millisecond of the cursor is in neither direction.
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_ROOT, true, false)
            .await
            .expect("the query runs"),
        "",
        "nothing older than the oldest post, and the oldest post is not older than itself"
    );

    // `DeleteAt = 0` is in the query, so the soft-deleted post is stepped over rather than
    // returned as a cursor a client would then fail to fetch.
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_BURN, false, false)
            .await
            .expect("the query runs"),
        LAST,
        "the deleted post between them is skipped"
    );
}

/// `collapsedThreads` narrows the cursor to roots — the one place in this pair of queries where
/// Go's builder assignment is not dropped on the floor, unlike `GetEtag`'s.
#[tokio::test]
async fn the_plain_cursor_skips_replies_in_collapsed_mode() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_ROOT, false, false)
            .await
            .expect("the query runs"),
        REPLY,
        "without the flag the reply is the next post"
    );
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_ROOT, false, true)
            .await
            .expect("the query runs"),
        BURN,
        "with it the reply is invisible and the next root wins"
    );

    // **And the same pair going backwards.** `before` and `after` are two separate statements
    // rather than one with a swapped comparison, so a fixture that only ever steps forwards
    // leaves half the code with no oracle — which is exactly what a mutation of the `DESC`
    // branch found here before these two assertions existed.
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_BURN, true, false)
            .await
            .expect("the query runs"),
        REPLY,
        "stepping back from the burn-on-read post reaches the reply"
    );
    assert_eq!(
        store
            .get_post_id_around_time(CHANNEL, T_BURN, true, true)
            .await
            .expect("the query runs"),
        ROOT,
        "and in collapsed mode it steps over the reply to its root"
    );
}

/// The predicate that gives `get_visible_post_id_around_time` its name, one branch at a time.
///
/// Four rows in the disjunction and each needs its own request: a fixture where the answer is the
/// same for two of them cannot tell which one produced it.
#[tokio::test]
async fn the_visible_cursor_steps_over_an_expired_burn_on_read_post() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    // 1. No receipt at all: the post is visible, so it is the cursor — in both directions.
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_REPLY, false, false, READER)
            .await
            .expect("the query runs"),
        BURN,
        "a burn-on-read post nobody has read yet is still visible"
    );
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_DELETED, true, false, READER)
            .await
            .expect("the query runs"),
        BURN,
        "and is what a backwards step lands on too"
    );

    // 2. A receipt that has not expired: still visible.
    read_receipt(&pool, BURN, READER, i64::MAX).await;
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_REPLY, false, false, READER)
            .await
            .expect("the query runs"),
        BURN,
        "an unexpired receipt does not hide the post"
    );

    // 3. A receipt that expired in 1970: the reader can no longer see it, so the cursor steps
    //    over it — and over the deleted post behind it — in one round trip.
    read_receipt(&pool, BURN, READER, 1).await;
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_REPLY, false, false, READER)
            .await
            .expect("the query runs"),
        LAST,
        "an expired receipt hides the post from this reader"
    );

    // 3b. The same step **backwards**, which is where `prev_post_id` comes from. The `before`
    //     query is its own statement, so the predicate has to be pinned in both directions or
    //     half of it has no oracle — a mutation of the `DESC` branch survived until this existed.
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_DELETED, true, false, READER)
            .await
            .expect("the query runs"),
        REPLY,
        "stepping back over the hidden post reaches the reply"
    );

    // 4. The author sees their own burn-on-read post regardless. Same row, same expired receipt,
    //    different caller — which is what makes this a test of the `UserId` branch and not of
    //    the `ExpireAt` one.
    read_receipt(&pool, BURN, AUTHOR, 1).await;
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_REPLY, false, false, AUTHOR)
            .await
            .expect("the query runs"),
        BURN,
        "the author is exempt"
    );

    // 5. And the type check comes first: an expired receipt against an ordinary post is not a
    //    reason to hide it. Nothing writes such a row today, which is exactly why the predicate's
    //    order has to be pinned rather than assumed.
    read_receipt(&pool, LAST, READER, 1).await;
    assert_eq!(
        store
            .get_visible_post_id_around_time(CHANNEL, T_DELETED, false, false, READER)
            .await
            .expect("the query runs"),
        LAST,
        "an expired receipt on a post that is not burn-on-read changes nothing"
    );
}

/// `get_etag` reports the newest `UpdateAt` in the channel, and `None` for a channel with no
/// posts — the branch that makes the caller stamp the clock instead.
#[tokio::test]
async fn the_etag_read_is_the_newest_update_at_or_nothing() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlPostStore::new(pool.clone());

    assert_eq!(
        store.get_etag(CHANNEL).await,
        Some(T_LAST),
        "the newest UpdateAt, deleted posts and replies included"
    );
    assert_eq!(
        store.get_etag("mmrspostpage00000000nosuch").await,
        None,
        "a channel with no posts has no stored etag"
    );
}
