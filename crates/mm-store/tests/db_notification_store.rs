//! The five store methods `App.SendNotifications` (channels/app/notification.go:53) reads and
//! writes, against a real Postgres: `SqlUserStore::get_all_profiles_in_channel`,
//! `SqlChannelStore::get_all_channel_members_notify_props_for_channel` and
//! `increment_mention_count`, `SqlThreadStore::get_thread_followers`, and the
//! `update_participants` branch of `SqlThreadStore::maintain_membership`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_notification_store
//! ```
//!
//! # Why these are here and not only in the parity suite
//!
//! None of the five is a route. Their results surface through `POST /posts` only as the side
//! effects of a notification — a counter on a membership row, a name in a participants list —
//! and the predicates that decide *who* (`Users.DeleteAt = 0` but no `Bots` filter; a `NULL`
//! `NotifyProps` still a key; `Following = true` only when asked; the participants append that
//! is **not** a move) are visible only where each query is called directly, with rows built on
//! both sides of each one.
//!
//! # Every assertion is scoped to the planted prefix
//!
//! Three of the reads are per-channel or per-thread, but the parity suite may be mid-run on the
//! same database and its own fixtures come and go; the sets asserted below are filtered to
//! `mmrsnotif%` ids before comparison, and the planted channels and threads are private to
//! this file so nothing else joins them. Every row is `mmrsnotif`-prefixed and swept first.

use std::collections::BTreeSet;

use mm_model::utils::StringMap;
use mm_store::channel_store::ChannelStore;
use mm_store::thread_store::{ThreadMembershipOpts, ThreadStore};
use mm_store::user_store::UserStore;
use mm_store::{SqlChannelStore, SqlThreadStore, SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PREFIX: &str = "mmrsnotif";

const CHANNEL: &str = "mmrsnotifchan0000000000001";
const OTHER_CHANNEL: &str = "mmrsnotifchan0000000000002";

/// Live members of `CHANNEL`. `LIVE_A`'s username sorts *after* `LIVE_B`'s, so a map built in
/// row order and a map built in id order are the same map — the sort is Go's, not observable.
const LIVE_A: &str = "mmrsnotifuser0000000000001";
const LIVE_B: &str = "mmrsnotifuser0000000000002";
/// A member whose `Users.DeleteAt` is non-zero.
const DELETED: &str = "mmrsnotifuser0000000000003";
/// A member with a `Bots` row.
const BOT: &str = "mmrsnotifuser0000000000004";
/// A live user who is a member of `OTHER_CHANNEL` only.
const OUTSIDER: &str = "mmrsnotifuser0000000000005";
/// A live user with no membership anywhere.
const NONMEMBER: &str = "mmrsnotifuser0000000000006";

const ROOT: &str = "mmrsnotifpost000000000root";
const OTHER_ROOT: &str = "mmrsnotifpost00000000root2";
/// A root with no `Threads` row — nobody has replied.
const LONELY_ROOT: &str = "mmrsnotifpost0000000lonely";
/// Followers of `ROOT`: two following, one not, one whose `Following` is `NULL`.
const FOLLOWER_A: &str = "mmrsnotifuser0000000000011";
const FOLLOWER_B: &str = "mmrsnotifuser0000000000012";
const UNFOLLOWER: &str = "mmrsnotifuser0000000000013";
const NULL_FOLLOWER: &str = "mmrsnotifuser0000000000014";
/// In `ROOT`'s participants list and has no membership row.
const PARTICIPANT_X: &str = "mmrsnotifuser0000000000021";
/// Has neither a membership row nor a place in the list.
const NEWCOMER: &str = "mmrsnotifuser0000000000022";

/// Planted counters, all distinct, so a `+1` on the wrong column is a different row.
const MENTIONS: i64 = 7;
const ROOT_MENTIONS: i64 = 5;
const URGENT_MENTIONS: i64 = 3;

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

async fn sweep(pool: &PgPool) {
    for statement in [
        "DELETE FROM threadmemberships WHERE postid LIKE 'mmrsnotif%' OR userid LIKE 'mmrsnotif%'",
        "DELETE FROM threads WHERE postid LIKE 'mmrsnotif%'",
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsnotif%' OR userid LIKE 'mmrsnotif%'",
        "DELETE FROM bots WHERE userid LIKE 'mmrsnotif%'",
        "DELETE FROM users WHERE id LIKE 'mmrsnotif%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("the sweep runs");
    }
}

/// Every string column Go scans is set, and every column `Sanitize` blanks is *non-empty*, so
/// "blanked" and "was never there" are different answers: `password`, `mfasecret`, `lastlogin`
/// and `mfausedtimestamps` all carry a value, and so do the columns the empty options map
/// leaves alone (`email`, `authservice`, `lastpasswordupdate`).
async fn plant_user(pool: &PgPool, id: &str, username: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                            authservice, email, emailverified, nickname, firstname, lastname,
                            position, roles, allowmarketing, props, notifyprops,
                            lastpasswordupdate, lastpictureupdate, failedattempts, locale,
                            timezone, mfaactive, mfasecret, mfausedtimestamps, remoteid,
                            lastlogin)
         VALUES ($1, 1000, 1000, $3, $2, 'hash', NULL, 'mmrsnotif-svc', $1 || '@mmrs.invalid',
                 true, '', '', '', '', 'system_user', false, '{}'::jsonb, '{}'::jsonb, 4321, 0,
                 0, 'en', '{}'::jsonb, false, 'mmrsnotif-mfa', '[\"1\"]'::jsonb, '', 77)",
    )
    .bind(id)
    .bind(username)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the user");
}

/// `notify_props` is bound as text and cast, so `None` is a SQL `NULL` and `Some("null")` is
/// the JSON value `null` — two rows Go's `StringMap.Scan` treats alike.
async fn plant_member(pool: &PgPool, channel_id: &str, user_id: &str, notify_props: Option<&str>) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                     mentioncount, mentioncountroot, msgcountroot,
                                     urgentmentioncount, notifyprops, lastupdateat,
                                     schemeuser, schemeadmin, schemeguest)
         VALUES ($1, $2, 'channel_user', 0, 15, $4, $5, 12, $6, $3::jsonb, 0, true, false, false)",
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(notify_props)
    .bind(MENTIONS)
    .bind(ROOT_MENTIONS)
    .bind(URGENT_MENTIONS)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

async fn plant_thread(pool: &PgPool, root: &str, participants: Option<&str>) {
    sqlx::query(
        "INSERT INTO threads (postid, replycount, lastreplyat, participants, channelid,
                              threaddeleteat, threadteamid)
         VALUES ($1, 2, 100, $2::jsonb, $3, 0, '')",
    )
    .bind(root)
    .bind(participants)
    .bind(CHANNEL)
    .execute(pool)
    .await
    .expect("inserts the thread");
}

async fn plant_follower(pool: &PgPool, root: &str, user_id: &str, following: Option<bool>) {
    sqlx::query(
        "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated,
                                        unreadmentions)
         VALUES ($1, $2, $3, 10, 42, 0)",
    )
    .bind(root)
    .bind(user_id)
    .bind(following)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

/// Six users, two channels, two threads, and memberships arranged so every predicate has a row
/// on each side of it.
async fn plant(pool: &PgPool) {
    sweep(pool).await;

    plant_user(pool, LIVE_A, "mmrsnotif-zed", 0).await;
    plant_user(pool, LIVE_B, "mmrsnotif-abe", 0).await;
    plant_user(pool, DELETED, "mmrsnotif-gone", 5).await;
    plant_user(pool, BOT, "mmrsnotif-bot", 0).await;
    plant_user(pool, OUTSIDER, "mmrsnotif-out", 0).await;
    plant_user(pool, NONMEMBER, "mmrsnotif-none", 0).await;
    sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat, lasticonupdate)
         VALUES ($1, 'mmrsnotif bot', $2, 1, 1, 0, 9)",
    )
    .bind(BOT)
    .bind(LIVE_A)
    .execute(pool)
    .await
    .expect("inserts the bot");

    plant_member(
        pool,
        CHANNEL,
        LIVE_A,
        Some(r#"{"push":"all","mmrsnotif":"x"}"#),
    )
    .await;
    plant_member(pool, CHANNEL, LIVE_B, None).await;
    plant_member(pool, CHANNEL, DELETED, Some(r#"{"desktop":"none"}"#)).await;
    plant_member(pool, CHANNEL, BOT, Some("null")).await;
    plant_member(pool, OTHER_CHANNEL, OUTSIDER, Some(r#"{"push":"mention"}"#)).await;
    plant_member(pool, OTHER_CHANNEL, LIVE_A, Some("{}")).await;

    plant_thread(
        pool,
        ROOT,
        Some(&format!(r#"["{PARTICIPANT_X}","{LIVE_A}"]"#)),
    )
    .await;
    plant_thread(pool, OTHER_ROOT, Some("[]")).await;
    plant_follower(pool, ROOT, FOLLOWER_A, Some(true)).await;
    plant_follower(pool, ROOT, FOLLOWER_B, Some(true)).await;
    plant_follower(pool, ROOT, UNFOLLOWER, Some(false)).await;
    plant_follower(pool, ROOT, NULL_FOLLOWER, None).await;
    plant_follower(pool, ROOT, LIVE_A, Some(true)).await;
    plant_follower(pool, OTHER_ROOT, FOLLOWER_A, Some(true)).await;
}

/// The planted ids in a result, as a set — everything else on the channel or thread belongs to
/// a suite that may be running beside this one.
fn ids<'a, I: IntoIterator<Item = &'a String>>(keys: I) -> BTreeSet<&'a str> {
    keys.into_iter()
        .map(String::as_str)
        .filter(|id| id.starts_with(PREFIX))
        .collect()
}

struct Counters {
    mentions: i64,
    root: i64,
    urgent: i64,
    last_update_at: i64,
}

async fn counters(pool: &PgPool, channel_id: &str, user_id: &str) -> Counters {
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT mentioncount, mentioncountroot, urgentmentioncount, lastupdateat
           FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("reads the membership");
    Counters {
        mentions: row.0,
        root: row.1,
        urgent: row.2,
        last_update_at: row.3,
    }
}

async fn participants(pool: &PgPool, root: &str) -> Option<Vec<String>> {
    let (value,): (Option<serde_json::Value>,) =
        sqlx::query_as("SELECT participants FROM threads WHERE postid = $1")
            .bind(root)
            .fetch_one(pool)
            .await
            .expect("reads the thread");
    value.map(|v| serde_json::from_value(v).expect("participants is a JSON array of strings"))
}

async fn has_membership(pool: &PgPool, root: &str, user_id: &str) -> bool {
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM threadmemberships WHERE postid = $1 AND userid = $2")
            .bind(root)
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("counts the membership");
    count == 1
}

// ---------------------------------------------------------------------------------------------
// GetAllProfilesInChannel
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn profiles_in_channel_drop_deleted_users_and_keep_bots() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .get_all_profiles_in_channel(CHANNEL, true)
        .await
        .expect("lists the channel's profiles");

    assert_eq!(
        ids(users.keys()),
        BTreeSet::from([LIVE_A, LIVE_B, BOT]),
        "live members and the bot; not the deleted member, the outsider or the non-member"
    );
    for (id, user) in &users {
        assert_eq!(id, &user.id, "the map is keyed by the user's own id");
    }
    let bot = &users[BOT];
    assert!(bot.is_bot, "the Bots join marks the bot");
    assert_eq!(bot.bot_description, "mmrsnotif bot");
    assert_eq!(bot.bot_last_icon_update, 9);
    assert!(!users[LIVE_A].is_bot);

    let other = store
        .get_all_profiles_in_channel(OTHER_CHANNEL, false)
        .await
        .expect("lists the other channel");
    assert_eq!(
        ids(other.keys()),
        BTreeSet::from([LIVE_A, OUTSIDER]),
        "scoped to the channel asked for, and `allow_from_cache` changes nothing"
    );
}

/// `Sanitize(map[string]bool{})` — the empty options map. The secrets go; the email, the auth
/// service and the password-update timestamp stay, because those are gated on options that an
/// empty map does not fail.
#[tokio::test]
async fn profiles_in_channel_are_sanitized_with_the_empty_options_map() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;

    let users = SqlUserStore::new(pool)
        .get_all_profiles_in_channel(CHANNEL, true)
        .await
        .expect("lists the channel's profiles");
    let user = &users[LIVE_A];

    assert_eq!(user.password, "", "Password is blanked");
    assert_eq!(user.mfa_secret, "", "MfaSecret is blanked");
    assert_eq!(user.mfa_used_timestamps, None, "MfaUsedTimestamps is nil");
    assert_eq!(user.last_login, 0, "LastLogin is zeroed");

    assert_eq!(
        user.email,
        format!("{LIVE_A}@mmrs.invalid"),
        "the email survives — SendNotifications reads it"
    );
    assert_eq!(user.auth_service, "mmrsnotif-svc", "AuthService survives");
    assert_eq!(
        user.last_password_update, 4321,
        "LastPasswordUpdate survives"
    );
    assert_eq!(user.username, "mmrsnotif-zed");
}

// ---------------------------------------------------------------------------------------------
// GetAllChannelMembersNotifyPropsForChannel
// ---------------------------------------------------------------------------------------------

/// No `Users` join: a member whose user is deleted is still in the map. A `NULL` column and a
/// JSON `null` are both an *empty* map under their key, never a missing key.
#[tokio::test]
async fn notify_props_are_keyed_by_member_and_null_is_an_empty_map() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlChannelStore::new(pool);

    let props = store
        .get_all_channel_members_notify_props_for_channel(CHANNEL, true)
        .await
        .expect("lists the members' notify props");

    assert_eq!(
        ids(props.keys()),
        BTreeSet::from([LIVE_A, LIVE_B, DELETED, BOT]),
        "every membership row on the channel, deleted user included; not the other channel's"
    );
    assert_eq!(
        props[LIVE_A],
        StringMap::from([
            ("push".to_owned(), "all".to_owned()),
            ("mmrsnotif".to_owned(), "x".to_owned()),
        ])
    );
    assert_eq!(
        props[DELETED],
        StringMap::from([("desktop".to_owned(), "none".to_owned())])
    );
    assert!(props[LIVE_B].is_empty(), "SQL NULL is an empty map");
    assert!(props[BOT].is_empty(), "JSON null is an empty map");

    let other = store
        .get_all_channel_members_notify_props_for_channel(OTHER_CHANNEL, false)
        .await
        .expect("lists the other channel");
    assert_eq!(ids(other.keys()), BTreeSet::from([LIVE_A, OUTSIDER]));
    assert_eq!(
        other[OUTSIDER],
        StringMap::from([("push".to_owned(), "mention".to_owned())])
    );
    assert!(other[LIVE_A].is_empty(), "'{{}}' is an empty map too");
}

// ---------------------------------------------------------------------------------------------
// IncrementMentionCount
// ---------------------------------------------------------------------------------------------

/// Runs one increment on a fresh plant and returns the four counters of `LIVE_A` on `CHANNEL`.
async fn increment(pool: &PgPool, user_ids: &[String], is_root: bool, is_urgent: bool) -> Counters {
    plant(pool).await;
    SqlChannelStore::new(pool.clone())
        .increment_mention_count(CHANNEL, user_ids, is_root, is_urgent)
        .await
        .expect("increments");
    counters(pool, CHANNEL, LIVE_A).await
}

fn planted() -> (i64, i64, i64) {
    (MENTIONS, ROOT_MENTIONS, URGENT_MENTIONS)
}

/// The four flag combinations move exactly the columns Go's `rootInc`/`urgentInc` say, and
/// `MentionCount` moves on all four.
#[tokio::test]
async fn increment_moves_the_columns_the_flags_name() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    let only_a = [LIVE_A.to_owned()];
    let (m, r, u) = planted();

    for (is_root, is_urgent, expected) in [
        (false, false, (m + 1, r, u)),
        (true, false, (m + 1, r + 1, u)),
        (false, true, (m + 1, r, u + 1)),
        (true, true, (m + 1, r + 1, u + 1)),
    ] {
        let after = increment(&pool, &only_a, is_root, is_urgent).await;
        assert_eq!(
            (after.mentions, after.root, after.urgent),
            expected,
            "is_root={is_root} is_urgent={is_urgent}"
        );
        assert!(
            after.last_update_at > 0,
            "LastUpdateAt moves to now on every touched row (is_root={is_root} is_urgent={is_urgent})"
        );
    }
}

/// Only the listed members of *this* channel move: not a member left off the list, not the same
/// user's membership on another channel, and a listed id with no membership is not an error.
#[tokio::test]
async fn increment_is_scoped_to_the_listed_members_of_the_channel() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    let store = SqlChannelStore::new(pool.clone());
    plant(&pool).await;
    let (m, r, u) = planted();

    store
        .increment_mention_count(
            CHANNEL,
            &[LIVE_A.to_owned(), DELETED.to_owned(), NONMEMBER.to_owned()],
            true,
            true,
        )
        .await
        .expect("a listed non-member is skipped, not an error");

    for user in [LIVE_A, DELETED] {
        let after = counters(&pool, CHANNEL, user).await;
        assert_eq!(
            (after.mentions, after.root, after.urgent),
            (m + 1, r + 1, u + 1),
            "{user}"
        );
        assert!(after.last_update_at > 0, "{user}");
    }
    for (channel, user) in [(CHANNEL, LIVE_B), (CHANNEL, BOT), (OTHER_CHANNEL, LIVE_A)] {
        let untouched = counters(&pool, channel, user).await;
        assert_eq!(
            (
                untouched.mentions,
                untouched.root,
                untouched.urgent,
                untouched.last_update_at
            ),
            (m, r, u, 0),
            "{user} on {channel} was not listed on this channel"
        );
    }
}

/// `sq.Eq{"UserId": []string{}}` renders `(1=0)`: nothing moves and nothing fails.
#[tokio::test]
async fn increment_with_no_user_ids_touches_nothing() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    let (m, r, u) = planted();

    let after = increment(&pool, &[], true, true).await;
    assert_eq!(
        (
            after.mentions,
            after.root,
            after.urgent,
            after.last_update_at
        ),
        (m, r, u, 0)
    );
}

// ---------------------------------------------------------------------------------------------
// GetThreadFollowers
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn thread_followers_filter_on_following_only_when_asked() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlThreadStore::new(pool);

    let active = store
        .get_thread_followers(ROOT, true)
        .await
        .expect("lists the active followers");
    assert_eq!(
        ids(&active),
        BTreeSet::from([FOLLOWER_A, FOLLOWER_B, LIVE_A]),
        "`Following = true` only: not the unfollower, and not a NULL `Following`"
    );

    let everyone = store
        .get_thread_followers(ROOT, false)
        .await
        .expect("lists every membership");
    assert_eq!(
        ids(&everyone),
        BTreeSet::from([FOLLOWER_A, FOLLOWER_B, UNFOLLOWER, NULL_FOLLOWER, LIVE_A]),
        "no filter at all — every row on the thread"
    );
    assert_eq!(
        ids(&everyone).len(),
        everyone.iter().filter(|id| id.starts_with(PREFIX)).count(),
        "one entry per membership row, no duplicates"
    );

    let other = store
        .get_thread_followers(OTHER_ROOT, true)
        .await
        .expect("lists the other thread");
    assert_eq!(
        ids(&other),
        BTreeSet::from([FOLLOWER_A]),
        "scoped to the thread"
    );

    let none = store
        .get_thread_followers(LONELY_ROOT, false)
        .await
        .expect("a thread with no memberships is an empty list, not NotFound");
    assert!(ids(&none).is_empty());
}

// ---------------------------------------------------------------------------------------------
// maintainMembershipTx with UpdateParticipants
// ---------------------------------------------------------------------------------------------

fn follow_with_participants(update_participants: bool) -> ThreadMembershipOpts {
    ThreadMembershipOpts {
        following: true,
        increment_mentions: false,
        update_following: true,
        update_viewed_timestamp: false,
        update_participants,
    }
}

/// A new membership row with the flag appends the user to the **end** of the list.
#[tokio::test]
async fn a_new_follower_with_the_flag_is_appended_to_participants() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;

    SqlThreadStore::new(pool.clone())
        .maintain_membership(NEWCOMER, ROOT, follow_with_participants(true))
        .await
        .expect("inserts the membership");

    assert!(
        has_membership(&pool, ROOT, NEWCOMER).await,
        "the membership row was inserted"
    );
    assert_eq!(
        participants(&pool, ROOT).await.as_deref(),
        Some(
            &[
                PARTICIPANT_X.to_owned(),
                LIVE_A.to_owned(),
                NEWCOMER.to_owned()
            ][..]
        ),
        "appended after the existing entries, which keep their order"
    );
    assert_eq!(
        participants(&pool, OTHER_ROOT).await.as_deref(),
        Some(&[][..]),
        "the other thread's list is untouched"
    );
}

/// `NOT participants ? user` — a user already in the list is left **where they are**. Go's
/// Postgres statement is an append guarded by membership, not a move to the end.
#[tokio::test]
async fn a_new_follower_already_in_participants_is_left_in_place() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;

    // `PARTICIPANT_X` is first in the list and has no membership row, so this is the insert
    // branch with the flag set — the one path that reaches the participants write.
    SqlThreadStore::new(pool.clone())
        .maintain_membership(PARTICIPANT_X, ROOT, follow_with_participants(true))
        .await
        .expect("inserts the membership");

    assert!(has_membership(&pool, ROOT, PARTICIPANT_X).await);
    assert_eq!(
        participants(&pool, ROOT).await.as_deref(),
        Some(&[PARTICIPANT_X.to_owned(), LIVE_A.to_owned()][..]),
        "still first, and not duplicated at the end"
    );
}

/// Without the flag the insert branch leaves the list alone.
#[tokio::test]
async fn a_new_follower_without_the_flag_leaves_participants_alone() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;

    SqlThreadStore::new(pool.clone())
        .maintain_membership(NEWCOMER, ROOT, follow_with_participants(false))
        .await
        .expect("inserts the membership");

    assert!(has_membership(&pool, ROOT, NEWCOMER).await);
    assert_eq!(
        participants(&pool, ROOT).await.as_deref(),
        Some(&[PARTICIPANT_X.to_owned(), LIVE_A.to_owned()][..])
    );
}

/// Go reads `UpdateParticipants` only after `saveMembership`, so an existing row — even one
/// missing from the list — is never added by the flag. `FOLLOWER_A` has a row and is not in the
/// list; the update branch runs (`UpdateViewedTimestamp` forces a write) and the list does not
/// change.
#[tokio::test]
async fn an_existing_follower_with_the_flag_is_not_added_to_participants() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;

    let membership = SqlThreadStore::new(pool.clone())
        .maintain_membership(
            FOLLOWER_A,
            ROOT,
            ThreadMembershipOpts {
                update_viewed_timestamp: true,
                ..follow_with_participants(true)
            },
        )
        .await
        .expect("updates the membership");
    assert!(
        membership.last_viewed > 10,
        "the update branch ran and wrote the row"
    );

    assert_eq!(
        participants(&pool, ROOT).await.as_deref(),
        Some(&[PARTICIPANT_X.to_owned(), LIVE_A.to_owned()][..]),
        "the update branch does not reach the participants write"
    );
}

/// A root nobody has replied to has no `Threads` row: the membership is still inserted and the
/// missing list is not an error. A `NULL` list stays `NULL` — `NULL || x` is `NULL` and the
/// guard is unknown, so the row is not matched, exactly as under Go.
#[tokio::test]
async fn a_missing_or_null_participants_list_is_not_an_error() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlThreadStore::new(pool.clone());

    store
        .maintain_membership(NEWCOMER, LONELY_ROOT, follow_with_participants(true))
        .await
        .expect("no Threads row is fine");
    assert!(has_membership(&pool, LONELY_ROOT, NEWCOMER).await);

    sqlx::query("UPDATE threads SET participants = NULL WHERE postid = $1")
        .bind(OTHER_ROOT)
        .execute(&pool)
        .await
        .expect("nulls the list");
    store
        .maintain_membership(NEWCOMER, OTHER_ROOT, follow_with_participants(true))
        .await
        .expect("a NULL list is fine");
    assert!(has_membership(&pool, OTHER_ROOT, NEWCOMER).await);
    assert_eq!(
        participants(&pool, OTHER_ROOT).await,
        None,
        "NULL stays NULL"
    );
}
