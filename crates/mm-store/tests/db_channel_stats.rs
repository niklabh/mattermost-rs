//! The four `getChannelStats` count queries against a real Postgres, on fixtures the REST API
//! cannot fully build.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_stats
//! ```
//!
//! # Why this exists next to `parity_channel_stats.rs`
//!
//! The parity suite compares both servers over HTTP, but three of the predicates here are hard or
//! impossible to reach that way on Team Edition:
//!
//! - **`SchemeGuest = TRUE` against a NULL column.** Guest accounts are config-gated
//!   (`EnableGuestAccounts`, off by default), so REST cannot mint a guest — and it certainly
//!   cannot mint a member whose `SchemeGuest` is SQL NULL, which is what rows predating the
//!   column hold. `NULL = TRUE` filtering the row *out* is the behaviour under test.
//! - **A deactivated guest.** Excluded by `Users.DeleteAt = 0` from both member and guest
//!   counts; building it over REST needs the guest feature first.
//! - **`FileInfo.PostId != ''`.** An uploaded-but-never-attached file. Reachable over REST in
//!   principle (upload without posting), but pinned here where the row's shape is explicit.
//!
//! These are transcriptions of channel_store.go:2646-2775 asserted against our own
//! implementation, the same standing weakening as [D-151]; the parity suite carries the
//! measured-against-Go half.
//!
//! # The counts are pairwise distinct by construction
//!
//! 3 members, 1 guest, 2 pinned posts, 4 files. The `getChannelUnread` session paid for the
//! lesson: two queries answering the same number cannot catch a handler wiring a result to the
//! wrong field, so the fixture makes every wrong wiring a different number.

use mm_store::channel_store::{
    get_file_count, get_guest_count, get_member_count, get_pinned_post_count,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The tests share one set of fixture rows and each purges before seeding; running two
/// interleaved would delete each other's rows mid-assertion. Same file-local serialisation as
/// every other DB suite (see the 2026-08-20 notes: a no-op mutation control failed on exactly
/// this race before the mutexes existed).
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsstatsteam000000000000t";
const CHANNEL: &str = "mmrsstatschan000000000main";
const OTHER_CHANNEL: &str = "mmrsstatschan00000000other";
const USER_PLAIN: &str = "mmrsstatsuser00000000actv1";
const USER_NULL_FLAG: &str = "mmrsstatsuser00000000actv2";
const USER_DEACTIVATED: &str = "mmrsstatsuser00000000deact";
const USER_GUEST: &str = "mmrsstatsuser00000000guest";
const USER_DEACTIVATED_GUEST: &str = "mmrsstatsuser0000000dguest";

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsstats%' OR userid LIKE 'mmrsstats%'",
        "DELETE FROM fileinfo WHERE id LIKE 'mmrsstats%'",
        "DELETE FROM posts WHERE id LIKE 'mmrsstats%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsstats%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsstats%'",
        "DELETE FROM users WHERE id LIKE 'mmrsstats%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn insert_user(pool: &PgPool, id: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, email, roles, lastlogin)
         VALUES ($1, 0, 0, $2, $1, $1 || '@mmrs.invalid', 'system_user', 0)",
    )
    .bind(id)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the user");
}

async fn insert_member(pool: &PgPool, channel_id: &str, user_id: &str, guest: Option<bool>) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                     mentioncount, mentioncountroot, msgcountroot,
                                     urgentmentioncount, notifyprops, lastupdateat,
                                     schemeuser, schemeadmin, schemeguest)
         VALUES ($1, $2, '', 0, 0, 0, 0, 0, 0, '{}'::jsonb, 0, true, false, $3)",
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(guest)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

async fn insert_post(pool: &PgPool, id: &str, channel_id: &str, pinned: bool, delete_at: i64) {
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, message, ispinned)
         VALUES ($1, 0, 0, $2, $3, $4, 'mmrs stats fixture', $5)",
    )
    .bind(id)
    .bind(delete_at)
    .bind(USER_PLAIN)
    .bind(channel_id)
    .bind(pinned)
    .execute(pool)
    .await
    .expect("inserts the post");
}

async fn insert_file(pool: &PgPool, id: &str, channel_id: &str, post_id: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO fileinfo (id, creatorid, postid, channelid, createat, updateat, deleteat,
                               path, name, extension, size, archived)
         VALUES ($1, $2, $3, $4, 0, 0, $5, '/mmrs', $1, 'txt', 1, false)",
    )
    .bind(id)
    .bind(USER_PLAIN)
    .bind(post_id)
    .bind(channel_id)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the file info");
}

/// One channel whose four counts are 3, 1, 2 and 4 — and a decoy for every predicate.
async fn seed(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
         VALUES ($1, 0, 0, 0, 'mmrs stats', 'mmrs-stats-team', 'O', false)",
    )
    .bind(TEAM)
    .execute(pool)
    .await
    .expect("inserts the team");

    for id in [CHANNEL, OTHER_CHANNEL] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, 0, $2, 'O'::channel_type, 'mmrs stats', $1, 0, 0)",
        )
        .bind(id)
        .bind(TEAM)
        .execute(pool)
        .await
        .expect("inserts the channel");
    }

    // Members: 3 count (plain, NULL-flag, guest); the two deactivated rows are the decoys.
    insert_user(pool, USER_PLAIN, 0).await;
    insert_user(pool, USER_NULL_FLAG, 0).await;
    insert_user(pool, USER_DEACTIVATED, 1_700_000_000_000).await;
    insert_user(pool, USER_GUEST, 0).await;
    insert_user(pool, USER_DEACTIVATED_GUEST, 1_700_000_000_000).await;

    insert_member(pool, CHANNEL, USER_PLAIN, Some(false)).await;
    insert_member(pool, CHANNEL, USER_NULL_FLAG, None).await; // NULL SchemeGuest
    insert_member(pool, CHANNEL, USER_DEACTIVATED, Some(false)).await;
    insert_member(pool, CHANNEL, USER_GUEST, Some(true)).await;
    insert_member(pool, CHANNEL, USER_DEACTIVATED_GUEST, Some(true)).await;
    // A member of the *other* channel, to give the channel-id predicate something to exclude.
    insert_member(pool, OTHER_CHANNEL, USER_PLAIN, Some(true)).await;

    // Posts: 2 pinned count; unpinned, deleted-pinned and other-channel-pinned are the decoys.
    insert_post(pool, "mmrsstatspost0000000000pn1", CHANNEL, true, 0).await;
    insert_post(pool, "mmrsstatspost0000000000pn2", CHANNEL, true, 0).await;
    insert_post(pool, "mmrsstatspost0000000unpind", CHANNEL, false, 0).await;
    insert_post(
        pool,
        "mmrsstatspost000000deleted",
        CHANNEL,
        true,
        1_700_000_000_000,
    )
    .await;
    insert_post(pool, "mmrsstatspost00000otherpin", OTHER_CHANNEL, true, 0).await;

    // Files: 4 count; the orphan (PostId = ''), the deleted one and the other-channel one are
    // the decoys.
    for id in [
        "mmrsstatsfile000000000att1",
        "mmrsstatsfile000000000att2",
        "mmrsstatsfile000000000att3",
        "mmrsstatsfile000000000att4",
    ] {
        insert_file(pool, id, CHANNEL, "mmrsstatspost0000000000pn1", 0).await;
    }
    insert_file(pool, "mmrsstatsfile0000000orphan", CHANNEL, "", 0).await;
    insert_file(
        pool,
        "mmrsstatsfile000000deleted",
        CHANNEL,
        "mmrsstatspost0000000000pn1",
        1_700_000_000_000,
    )
    .await;
    insert_file(
        pool,
        "mmrsstatsfile0000000other0",
        OTHER_CHANNEL,
        "mmrsstatspost00000otherpin",
        0,
    )
    .await;
}

/// All four counts on the same fixture, pairwise distinct, each excluding its decoys.
#[tokio::test]
async fn the_four_counts_are_distinct_and_each_excludes_its_decoys() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let members = get_member_count(&pool, CHANNEL).await.expect("counts");
    let guests = get_guest_count(&pool, CHANNEL).await.expect("counts");
    let pinned = get_pinned_post_count(&pool, CHANNEL).await.expect("counts");
    let files = get_file_count(&pool, CHANNEL).await.expect("counts");

    assert_eq!(
        members, 3,
        "plain + NULL-flag + guest; both deactivated users and the other channel's member are out"
    );
    assert_eq!(
        guests, 1,
        "SchemeGuest NULL and false are not guests, and a deactivated guest does not count"
    );
    assert_eq!(
        pinned, 2,
        "the unpinned, the deleted-but-pinned and the other channel's pin are out"
    );
    assert_eq!(
        files, 4,
        "the orphan (PostId = ''), the deleted file and the other channel's file are out"
    );

    let counts = [members, guests, pinned, files];
    for i in 0..counts.len() {
        for j in (i + 1)..counts.len() {
            assert_ne!(
                counts[i], counts[j],
                "two equal counts cannot catch a swapped wiring — fix the fixture"
            );
        }
    }

    purge(&pool).await;
}

/// A well-formed id that matches nothing is four zeroes, not an error: `COUNT(*)` has no
/// not-found case, which is why `getChannelStats` can 200 on a missing channel for an admin.
#[tokio::test]
async fn a_missing_channel_counts_zero_everywhere() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let missing = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    assert_eq!(get_member_count(&pool, missing).await.expect("counts"), 0);
    assert_eq!(get_guest_count(&pool, missing).await.expect("counts"), 0);
    assert_eq!(
        get_pinned_post_count(&pool, missing).await.expect("counts"),
        0
    );
    assert_eq!(get_file_count(&pool, missing).await.expect("counts"), 0);

    purge(&pool).await;
}
