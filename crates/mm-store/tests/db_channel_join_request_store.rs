//! `SqlChannelJoinRequestStore` against a real Postgres: the ordering, the two pagination clamps,
//! the status filter and the partial unique index.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_join_request_store
//! ```
//!
//! # The fixture is built so that the two sort keys disagree
//!
//! `ORDER BY createat DESC, id DESC` is only testable if each key alone would produce a
//! *different* answer. So two of the four rows share a `createat` and their ids break the tie in
//! the opposite direction from the third key a reader might reach for:
//!
//! | id | createat | by createat DESC | by id DESC |
//! |---|---|---|---|
//! | `…aaa` | 3000 | 1st | 4th |
//! | `…zzz` | 2000 | 2nd (tie) | 1st |
//! | `…mmm` | 2000 | 3rd (tie) | 2nd |
//! | `…bbb` | 1000 | 4th | 3rd |
//!
//! The tied pair is what makes the `Id DESC` half of the key observable at all: with four distinct
//! timestamps it never runs, and a store that dropped it would pass. An `ASC` in either position
//! reverses its half.
//!
//! # The unique index is partial, and that is the whole point of testing it
//!
//! `UNIQUE (ChannelId, UserId) WHERE Status = 'pending'`. So a second *pending* row for one
//! `(channel, user)` is refused — [`StoreError::Conflict`] — while a second **withdrawn** one is
//! accepted, which is exactly what lets a user request, withdraw and request again.
//!
//! # Every test here is named `join_request_store_*`
//!
//! `MUTATE_FILTER` selects test **names**, not files.

use mm_model::channel_join_request::{ChannelJoinRequest, GetChannelJoinRequestsOpts};
use mm_store::channel_join_request_store::ChannelJoinRequestStore;
use mm_store::{SqlChannelJoinRequestStore, StoreError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A channel id that names no channel, and a user id that names no user. Neither table is joined,
/// so the rows are reachable without them — and planting real ones would put a channel with no
/// team into lists that other suites byte-compare.
const CHANNEL: &str = "mmrsjrstorechan00000000001";
const OTHER_CHANNEL: &str = "mmrsjrstorechan00000000002";
const USER: &str = "mmrsjrstoreuser00000000001";

const NEWEST: &str = "mmrsjrstore000000000000aaa";
const TIED_HIGH: &str = "mmrsjrstore000000000000zzz";
const TIED_LOW: &str = "mmrsjrstore000000000000mmm";
const OLDEST: &str = "mmrsjrstore000000000000bbb";

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

/// Scoped to this file's own id prefix **and** its two synthetic channels: the parity binary
/// plants rows of its own through the API and runs concurrently with this one.
async fn purge(pool: &PgPool) {
    sqlx::query(
        "DELETE FROM channeljoinrequests \
         WHERE id LIKE 'mmrsjrstore%' OR channelid = $1 OR channelid = $2 OR userid = $3",
    )
    .bind(CHANNEL)
    .bind(OTHER_CHANNEL)
    .bind(USER)
    .execute(pool)
    .await
    .expect("purges leftover test rows");
}

/// One row, with every column given explicitly so the store's own `PreSave` is bypassed.
#[allow(clippy::too_many_arguments)]
async fn plant(
    pool: &PgPool,
    id: &str,
    channel_id: &str,
    user_id: &str,
    status: &str,
    create_at: i64,
) {
    sqlx::query(
        "INSERT INTO channeljoinrequests \
         (id, channelid, userid, message, status, denialreason, createat, updateat, reviewedby, reviewedat) \
         VALUES ($1, $2, $3, $4, $5, '', $6, $6, '', 0)",
    )
    .bind(id)
    .bind(channel_id)
    .bind(user_id)
    .bind(format!("note-{id}"))
    .bind(status)
    .bind(create_at)
    .execute(pool)
    .await
    .expect("plants a join request row");
}

/// The four-row fixture described in the module docs, all pending, all on [`CHANNEL`] but each
/// belonging to a different user so the partial unique index does not refuse them.
///
/// **The tied pair is planted low id first**, and that is the whole reason this file can see the
/// `Id DESC` half of the sort key. A mutation dropping it survived the first run: with no tiebreak
/// the query is a seq scan and a sort, and Postgres's sort is stable at this size — so the tied
/// rows come back in *insertion* order, and inserting `zzz` before `mmm` made that the same answer
/// `Id DESC` gives. The right answer and the wrong answer coincided. Measured directly against
/// Postgres, not guessed: planting `mmm` first makes the two disagree.
async fn seed(pool: &PgPool) {
    purge(pool).await;
    plant(
        pool,
        NEWEST,
        CHANNEL,
        "mmrsjrstoreuserA0000000001",
        "pending",
        3000,
    )
    .await;
    plant(
        pool,
        TIED_LOW,
        CHANNEL,
        "mmrsjrstoreuserC0000000001",
        "pending",
        2000,
    )
    .await;
    plant(
        pool,
        TIED_HIGH,
        CHANNEL,
        "mmrsjrstoreuserB0000000001",
        "pending",
        2000,
    )
    .await;
    plant(
        pool,
        OLDEST,
        CHANNEL,
        "mmrsjrstoreuserD0000000001",
        "pending",
        1000,
    )
    .await;
}

fn opts(status: &str, page: i64, per_page: i64) -> GetChannelJoinRequestsOpts {
    GetChannelJoinRequestsOpts {
        status: status.to_owned(),
        page,
        per_page,
    }
}

fn ids(rows: &[ChannelJoinRequest]) -> Vec<&str> {
    rows.iter().map(|r| r.id.as_str()).collect()
}

#[tokio::test]
async fn join_request_store_orders_by_create_at_desc_then_id_desc() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlChannelJoinRequestStore::new(pool.clone());

    let (rows, total) = store
        .get_for_channel(CHANNEL, &opts("", 0, 60))
        .await
        .expect("the list succeeds");

    assert_eq!(
        ids(&rows),
        vec![NEWEST, TIED_HIGH, TIED_LOW, OLDEST],
        "newest first, and the tied pair breaks on id DESC"
    );
    assert_eq!(total, 4, "the total is unpaginated");

    purge(&pool).await;
}

#[tokio::test]
async fn join_request_store_pages_without_moving_the_total() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlChannelJoinRequestStore::new(pool.clone());

    for (page, expected) in [
        (0, vec![NEWEST, TIED_HIGH]),
        (1, vec![TIED_LOW, OLDEST]),
        (2, vec![]),
    ] {
        let (rows, total) = store
            .get_for_channel(CHANNEL, &opts("", page, 2))
            .await
            .expect("the list succeeds");
        assert_eq!(ids(&rows), expected, "page {page}");
        assert_eq!(total, 4, "page {page}: the total ignores the page");
    }

    // The store's own clamp: `per_page <= 0` is **60**, so every row comes back rather than none.
    let (rows, _) = store
        .get_for_channel(CHANNEL, &opts("", 0, 0))
        .await
        .expect("the list succeeds");
    assert_eq!(rows.len(), 4, "a zero per_page takes the store's default");

    // And a negative page is the first one, not a negative offset Postgres would refuse.
    let (rows, _) = store
        .get_for_channel(CHANNEL, &opts("", -3, 2))
        .await
        .expect("the list succeeds");
    assert_eq!(ids(&rows), vec![NEWEST, TIED_HIGH], "a negative page");

    purge(&pool).await;
}

#[tokio::test]
async fn join_request_store_filters_on_status_and_defaults_to_pending() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    // Two rows on one channel for **one** user: only one may be pending.
    plant(&pool, NEWEST, CHANNEL, USER, "pending", 3000).await;
    plant(&pool, OLDEST, CHANNEL, USER, "denied", 1000).await;
    let store = SqlChannelJoinRequestStore::new(pool.clone());

    let (rows, total) = store
        .get_for_channel(CHANNEL, &opts("", 0, 60))
        .await
        .expect("the list succeeds");
    assert_eq!(ids(&rows), vec![NEWEST], "an empty status means pending");
    assert_eq!(total, 1, "and the count is filtered the same way");

    let (rows, total) = store
        .get_for_channel(CHANNEL, &opts("denied", 0, 60))
        .await
        .expect("the list succeeds");
    assert_eq!(ids(&rows), vec![OLDEST]);
    assert_eq!(total, 1);

    // The store validates nothing: an unrecognised status matches no row.
    let (rows, total) = store
        .get_for_channel(CHANNEL, &opts("bogus", 0, 60))
        .await
        .expect("the list succeeds");
    assert!(rows.is_empty());
    assert_eq!(total, 0);

    // `count_pending` names the status itself, so `?status=` cannot move it.
    assert_eq!(
        store.count_pending(CHANNEL).await.expect("counts"),
        1,
        "only the pending row"
    );

    // The user scope is a different column and the same predicate.
    let (rows, total) = store
        .get_for_user(USER, &opts("", 0, 60))
        .await
        .expect("the list succeeds");
    assert_eq!(ids(&rows), vec![NEWEST], "by user, pending only");
    assert_eq!(total, 1);

    purge(&pool).await;
}

#[tokio::test]
async fn join_request_store_refuses_a_second_pending_row_and_allows_a_second_terminal_one() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlChannelJoinRequestStore::new(pool.clone());

    let mut first = ChannelJoinRequest {
        channel_id: CHANNEL.to_owned(),
        user_id: USER.to_owned(),
        message: "first".to_owned(),
        ..Default::default()
    };
    store.save(&mut first).await.expect("the first save works");
    assert!(!first.id.is_empty(), "PreSave mints the id in place");
    assert_eq!(first.status, "pending", "and defaults the status");
    assert_eq!(
        first.update_at, first.create_at,
        "PreSave ties update_at to create_at"
    );

    let mut second = ChannelJoinRequest {
        channel_id: CHANNEL.to_owned(),
        user_id: USER.to_owned(),
        message: "second".to_owned(),
        ..Default::default()
    };
    assert!(
        matches!(
            store.save(&mut second).await,
            Err(StoreError::Conflict {
                resource: "ChannelJoinRequest",
                ..
            })
        ),
        "the partial unique index refuses a second pending row"
    );

    // The same user on a **different** channel is not a conflict — the index is on the pair.
    let mut elsewhere = ChannelJoinRequest {
        channel_id: OTHER_CHANNEL.to_owned(),
        user_id: USER.to_owned(),
        ..Default::default()
    };
    store
        .save(&mut elsewhere)
        .await
        .expect("a different channel is not a conflict");

    // Withdraw the first, and the same pair becomes free again.
    //
    // The row is rewound to a 1970 `create_at`/`update_at` first, so `PreUpdate`'s fresh stamp is
    // observable: without it `update_at` stays at 1000 and the assertion below fails. With the
    // row's own clock values — minted seconds ago — a dropped `PreUpdate` is indistinguishable.
    sqlx::query("UPDATE channeljoinrequests SET createat = 1000, updateat = 1000 WHERE id = $1")
        .bind(&first.id)
        .execute(&pool)
        .await
        .expect("rewinds the clock");
    let mut first = store
        .get(&first.id)
        .await
        .expect("re-reads the rewound row");
    assert_eq!(first.update_at, 1000, "the rewind took");
    first.status = "withdrawn".to_owned();
    first.message = String::new();
    store.update(&mut first).await.expect("the update works");
    assert!(
        first.update_at > 1_600_000_000_000,
        "PreUpdate stamps a fresh update_at, not the row's old one: {}",
        first.update_at
    );
    assert_eq!(first.create_at, 1000, "and leaves create_at alone");

    // The status really reached the column — a store that dropped `Status` from the `SET` list
    // would leave a pending row, and the save below would conflict.
    assert_eq!(
        store.get(&first.id).await.expect("re-reads").status,
        "withdrawn"
    );

    let mut again = ChannelJoinRequest {
        channel_id: CHANNEL.to_owned(),
        user_id: USER.to_owned(),
        message: "again".to_owned(),
        ..Default::default()
    };
    store
        .save(&mut again)
        .await
        .expect("a withdrawn row frees the pair");

    // The withdrawn row is still there — nothing is ever deleted.
    let (_, total) = store
        .get_for_user(USER, &opts("withdrawn", 0, 60))
        .await
        .expect("the list succeeds");
    assert_eq!(total, 1, "the withdrawn row survives");

    purge(&pool).await;
}

#[tokio::test]
async fn join_request_store_misses_are_not_found_and_an_update_of_nothing_is_too() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let store = SqlChannelJoinRequestStore::new(pool.clone());

    assert!(matches!(
        store.get("mmrsjrstore000000000000nil").await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.get_pending_for_channel_and_user(CHANNEL, USER).await,
        Err(StoreError::NotFound { .. })
    ));

    // `Get` finds a **non-pending** row; `GetPendingForChannelAndUser` does not.
    plant(&pool, OLDEST, CHANNEL, USER, "denied", 1000).await;
    assert_eq!(
        store.get(OLDEST).await.expect("the row is there").status,
        "denied"
    );
    assert!(matches!(
        store.get_pending_for_channel_and_user(CHANNEL, USER).await,
        Err(StoreError::NotFound { .. })
    ));

    // An update whose `WHERE Id` matches nothing is a miss, not a silent success.
    let mut vanished = ChannelJoinRequest {
        id: "mmrsjrstore000000000000nil".to_owned(),
        channel_id: CHANNEL.to_owned(),
        user_id: USER.to_owned(),
        status: "withdrawn".to_owned(),
        create_at: 1,
        update_at: 1,
        ..Default::default()
    };
    assert!(matches!(
        store.update(&mut vanished).await,
        Err(StoreError::NotFound { .. })
    ));

    // And `IsValid` runs before the query: an invalid row never reaches Postgres.
    let mut invalid = ChannelJoinRequest {
        channel_id: CHANNEL.to_owned(),
        user_id: "not-an-id".to_owned(),
        ..Default::default()
    };
    assert!(matches!(
        store.save(&mut invalid).await,
        Err(StoreError::Invalid {
            entity: "ChannelJoinRequest",
            ..
        })
    ));

    purge(&pool).await;
}
