//! The two channel-store halves of `POST /channels/members/{user_id}/view`, against a real
//! Postgres: `get_channels_with_unreads_and_with_mentions` and `update_last_viewed_at`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_view_reads
//! ```
//!
//! # What the cross-server parity suite cannot reach
//!
//! `crates/mm-api/tests/parity/channel_view.rs` drives both servers over HTTP and covers the
//! wire format, the ordering of the refusals and the read-state a client can observe. Four things
//! are invisible to it and are the reason this file exists:
//!
//! - **The `Channels.Type <> 'S'` deny-list**, and the fact that it is a deny-list of exactly one
//!   type rather than the `IN (O, P, D, G)` allow-list every neighbouring query uses. A space
//!   cannot be created through the REST API on Team Edition; a board can only be reached through
//!   a 400 the handler raises before this query runs. So over HTTP, widening the filter to the
//!   allow-list — which would silently drop board read-state — passes everything.
//! - **`UpdateLastViewedAt`'s return value comes from `Channels`, not from `ChannelMembers`.** An
//!   id with no membership row is in the answer; the app layer discards that map, so no wire
//!   surface distinguishes it.
//! - **The `ErrInvalidInput` that only a wholly-unmatched id list produces**, which is the
//!   difference between a 400 and a 500 on the route — and which the handler's own id validation
//!   makes hard to reach with a well-formed request.
//! - **`LastUpdateAt` tracking `LastViewedAt`.** Not on this route's response.
//!
//! Every row here is `mmrsview`-prefixed and purged before and after.

use std::collections::BTreeMap;

use mm_model::utils::StringMap;
use mm_store::channel_store::{
    get_channels_with_unreads_and_with_mentions, get_direct_messages_with_unread_and_mentions,
    get_team_channels_with_unread_and_mentions, update_last_viewed_at,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// As in the other store suites: the fixtures are a fixed id set, so two of these running
/// concurrently would delete each other's rows mid-assertion.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsviewteam0000000000team";
const USER: &str = "mmrsviewuser0000000000user";
/// Unread, `push` unset on the membership.
const UNREAD: &str = "mmrsviewchan000000000unrd";
/// Fully read: `MsgCount` equals the channel's `TotalMsgCount` and no mentions.
const READ: &str = "mmrsviewchan00000000read0";
/// A board — in scope for this query, unlike every neighbouring one.
const BOARD: &str = "mmrsviewchan0000000board0";
/// A space — the one type the deny-list excludes.
const SPACE: &str = "mmrsviewchan0000000space0";
/// A direct channel, which the mentions branch treats as `all` whatever its prop says.
const DIRECT: &str = "mmrsviewchan000000direct0";

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsview%' OR userid LIKE 'mmrsview%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsview%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsview%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Five channels whose counters differ **only** where the assertion needs them to.
///
/// `LastPostAt` is distinct per channel (1000, 2000, …) and `LastViewedAt` is deliberately larger
/// than `LastPostAt` on exactly one of them, so `max(LastPostAt, LastViewedAt)` cannot be
/// mistaken for either column on its own.
async fn seed(pool: &PgPool, membership_push: &str) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
         VALUES ($1, 0, 0, 0, 'mmrs view', 'mmrs-view-team', 'O', false)",
    )
    .bind(TEAM)
    .execute(pool)
    .await
    .expect("inserts the team");

    // (id, type, total_msg_count, member_msg_count, mention_count, last_post_at, last_viewed_at)
    for (id, channel_type, total, mine, mentions, last_post_at, last_viewed_at) in [
        (UNREAD, "O", 40_i64, 15_i64, 4_i64, 1000_i64, 500_i64),
        (READ, "O", 40, 40, 0, 2000, 9000),
        (BOARD, "BO", 40, 15, 0, 3000, 0),
        (SPACE, "S", 40, 15, 0, 4000, 0),
        (DIRECT, "D", 40, 15, 0, 5000, 0),
    ] {
        // **The direct channel gets no team**, which is what a real `D` row looks like and what
        // makes the team-scoped query and the DM-scoped one disjoint. A space would not really
        // share the team either, but nothing here reads its `TeamId` and giving it one keeps the
        // deny-list the only thing separating it from its neighbours.
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot, lastpostat)
             VALUES ($1, 0, 0, 0, $2, $3::channel_type, 'mmrs view', $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(if id == DIRECT { "" } else { TEAM })
        .bind(channel_type)
        .bind(format!("mmrs-view-{id}"))
        .bind(total)
        .bind(total - 10)
        .bind(last_post_at)
        .execute(pool)
        .await
        .expect("inserts the channel");

        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                         mentioncount, mentioncountroot, msgcountroot,
                                         urgentmentioncount, notifyprops, lastupdateat,
                                         schemeuser, schemeadmin, schemeguest)
             VALUES ($1, $2, 'channel_user', $3, $4, $5, $5, $6, $5,
                     jsonb_build_object('push', $7::text), 0, true, false, false)",
        )
        .bind(id)
        .bind(USER)
        .bind(last_viewed_at)
        .bind(mine)
        .bind(mentions)
        .bind((mine - 5).max(0))
        .bind(membership_push)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
}

fn all_ids() -> Vec<String> {
    [UNREAD, READ, BOARD, SPACE, DIRECT]
        .iter()
        .map(|id| (*id).to_owned())
        .collect()
}

/// **A space is excluded and a board is not.**
///
/// The single claim this whole file was written for. `nonMessageBackingChannelTypes`
/// (channel_store.go:52) holds `S` alone, so `BO` is read-state the view route marks; every
/// neighbouring channel query would call the same channel missing.
#[tokio::test]
async fn store_channel_view_deny_list_excludes_spaces_and_keeps_boards() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    let result = get_channels_with_unreads_and_with_mentions(&pool, &all_ids(), USER, None)
        .await
        .expect("the query runs");

    purge(&pool).await;

    let seen: Vec<&str> = result.read_times.keys().map(String::as_str).collect();
    assert!(seen.contains(&BOARD), "a board is in scope: {seen:?}");
    assert!(!seen.contains(&SPACE), "a space is not: {seen:?}");
    assert_eq!(seen.len(), 4, "five channels, one excluded: {seen:?}");
}

/// `read_times` is `max(LastPostAt, LastViewedAt)` **for every membership**, including the
/// channels that are fully read and are therefore not in `with_unreads`.
///
/// `READ` is the row where `LastViewedAt` (9000) exceeds `LastPostAt` (2000); every other row is
/// the other way round. A port that returned either column alone fails on one of the two.
#[tokio::test]
async fn store_channel_view_read_times_is_the_larger_of_the_two_columns() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    let result = get_channels_with_unreads_and_with_mentions(&pool, &all_ids(), USER, None)
        .await
        .expect("the query runs");

    purge(&pool).await;

    assert_eq!(
        result.read_times.get(UNREAD),
        Some(&1000),
        "LastPostAt wins"
    );
    assert_eq!(
        result.read_times.get(READ),
        Some(&9000),
        "LastViewedAt wins"
    );
    assert_eq!(result.read_times.get(BOARD), Some(&3000));
    assert_eq!(result.read_times.get(DIRECT), Some(&5000));

    let unreads: Vec<&str> = result.with_unreads.iter().map(String::as_str).collect();
    assert!(
        !unreads.contains(&READ),
        "a caught-up membership is not unread: {unreads:?}"
    );
    assert_eq!(unreads.len(), 3, "unread, board and direct: {unreads:?}");
}

/// **A direct channel is in `with_mentions` on its type alone**, with `push` set to `none` on
/// both the membership and the user — which puts every other unread channel out.
///
/// This is the one branch of the classification with no wire surface at all in this port (there
/// is no push hub to clear), so it is asserted here or nowhere.
#[tokio::test]
async fn store_channel_view_a_direct_channel_is_a_mention_whatever_the_prop_says() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "none").await;

    let user_props = StringMap::from(BTreeMap::from([("push".to_owned(), "none".to_owned())]));
    let result =
        get_channels_with_unreads_and_with_mentions(&pool, &all_ids(), USER, Some(&user_props))
            .await
            .expect("the query runs");

    purge(&pool).await;

    let mentions: Vec<&str> = result.with_mentions.iter().map(String::as_str).collect();
    assert_eq!(
        mentions,
        vec![DIRECT],
        "only the direct channel, and it is unread"
    );
}

/// The membership's `push` is `default`, so the **user's** prop decides — and `all` puts every
/// unread channel in, not only the direct one.
#[tokio::test]
async fn store_channel_view_a_default_membership_prop_falls_back_to_the_users() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    let user_props = StringMap::from(BTreeMap::from([("push".to_owned(), "all".to_owned())]));
    let result =
        get_channels_with_unreads_and_with_mentions(&pool, &all_ids(), USER, Some(&user_props))
            .await
            .expect("the query runs");

    purge(&pool).await;

    let mut mentions: Vec<&str> = result.with_mentions.iter().map(String::as_str).collect();
    mentions.sort_unstable();
    let mut expected = vec![UNREAD, BOARD, DIRECT];
    expected.sort_unstable();
    assert_eq!(mentions, expected, "every unread channel, `all` being set");
}

/// The write, and the three things about it that no route can show.
///
/// - The answer is keyed by **channel**, so `READ` — whose membership the statement leaves
///   untouched, both `greatest`es being no-ops — is still in it, carrying its `LastPostAt`.
/// - `MsgCount`, `LastViewedAt` and `LastUpdateAt` move to the channel's values, and the three
///   mention counters go to zero.
/// - `LastUpdateAt` equals the **new** `LastViewedAt`, not a fresh clock reading.
#[tokio::test]
async fn store_channel_view_update_pulls_the_membership_forward() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    let times = update_last_viewed_at(&pool, &[UNREAD.to_owned(), READ.to_owned()], USER)
        .await
        .expect("the update runs");

    let row = sqlx::query!(
        r#"SELECT msgcount AS "msgcount!", msgcountroot AS "msgcountroot!",
                  mentioncount AS "mentioncount!", mentioncountroot AS "mentioncountroot!",
                  urgentmentioncount AS "urgentmentioncount!",
                  lastviewedat AS "lastviewedat!", lastupdateat AS "lastupdateat!"
             FROM channelmembers WHERE channelid = $1 AND userid = $2"#,
        UNREAD,
        USER
    )
    .fetch_one(&pool)
    .await
    .expect("the membership is still there");

    // `READ`'s membership is ahead of its channel on every column, so the statement must not
    // move it backwards.
    let untouched = sqlx::query!(
        r#"SELECT msgcount AS "msgcount!", lastviewedat AS "lastviewedat!"
             FROM channelmembers WHERE channelid = $1 AND userid = $2"#,
        READ,
        USER
    )
    .fetch_one(&pool)
    .await
    .expect("the read membership is still there");

    purge(&pool).await;

    assert_eq!(times.get(UNREAD), Some(&1000), "the channel's LastPostAt");
    assert_eq!(
        times.get(READ),
        Some(&2000),
        "a membership the statement did not move is still in the answer"
    );

    assert_eq!(row.msgcount, 40, "greatest(15, 40)");
    assert_eq!(row.msgcountroot, 30, "greatest(10, 30)");
    assert_eq!(row.mentioncount, 0, "the fixture's 4 is zeroed");
    assert_eq!(row.mentioncountroot, 0, "the fixture's 4 is zeroed");
    assert_eq!(row.urgentmentioncount, 0, "the fixture's 4 is zeroed");
    assert_eq!(row.lastviewedat, 1000, "greatest(500, 1000)");
    assert_eq!(
        row.lastupdateat, row.lastviewedat,
        "LastUpdateAt is set from LastViewedAt, not from the clock"
    );

    assert_eq!(untouched.msgcount, 40, "greatest(40, 40) is a no-op");
    assert_eq!(
        untouched.lastviewedat, 9000,
        "greatest(9000, 2000) — the statement never moves a membership backwards"
    );
}

/// **An id that matches no `Channels` row at all is the `ErrInvalidInput` branch** — and one that
/// matches a channel the user is not a member of is not. The app layer turns the first into a
/// 400 and everything else into a 500, so the two must not be folded together.
#[tokio::test]
async fn store_channel_view_only_a_wholly_unmatched_id_list_is_invalid_input() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    // A real channel, but asked about on behalf of a user with no membership row: the UPDATE
    // touches nothing and the SELECT still answers.
    let stranger = update_last_viewed_at(&pool, &[UNREAD.to_owned()], "mmrsviewnobody00000000nob")
        .await
        .expect("a non-member is not an error");
    assert_eq!(stranger.get(UNREAD), Some(&1000));

    let missing = update_last_viewed_at(&pool, &["mmrsviewnosuchchannel00000".to_owned()], USER)
        .await
        .expect_err("no Channels row at all is ErrInvalidInput");

    purge(&pool).await;

    assert!(
        matches!(
            missing,
            mm_store::StoreError::InvalidInput {
                entity: "Channel",
                field: "Id",
                ..
            }
        ),
        "got {missing:?}"
    );
}

/// An empty id list is an empty map with **no statement sent** — not the `ErrInvalidInput` that
/// an unmatched list produces. Go returns before it builds the CTE.
#[tokio::test]
async fn store_channel_view_an_empty_id_list_is_an_empty_map() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    let times = update_last_viewed_at(&pool, &[], USER)
        .await
        .expect("an empty list is not an error");
    assert!(times.is_empty());
}

/// **The team query and the DM query partition the same memberships.** Between them they see
/// every channel the id-list query does, and neither sees the other's.
///
/// The team query keeps its one-type deny-list, so the board is a team channel; the DM query has
/// an allow-list (`Type IN (D, G)`) instead and no team predicate at all, because a direct
/// channel carries no `TeamId`.
#[tokio::test]
async fn store_channel_view_team_and_direct_queries_partition_the_memberships() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, "default").await;

    let team = get_team_channels_with_unread_and_mentions(&pool, TEAM, USER, None)
        .await
        .expect("the team query runs");
    let direct = get_direct_messages_with_unread_and_mentions(&pool, USER, None)
        .await
        .expect("the direct query runs");

    purge(&pool).await;

    let mut in_team: Vec<&str> = team.read_times.keys().map(String::as_str).collect();
    in_team.sort_unstable();
    let mut expected = vec![UNREAD, READ, BOARD];
    expected.sort_unstable();
    assert_eq!(
        in_team, expected,
        "the board is a team channel; the space is denied and the DM has no team"
    );

    let in_direct: Vec<&str> = direct.read_times.keys().map(String::as_str).collect();
    assert_eq!(
        in_direct,
        vec![DIRECT],
        "and the DM is only in the other one"
    );

    // The classification is the same code, so the only thing worth re-asserting is that it ran.
    assert_eq!(
        team.read_times.get(READ),
        Some(&9000),
        "still max(LastPostAt, LastViewedAt)"
    );
    assert_eq!(direct.with_unreads, vec![DIRECT.to_owned()]);
}
