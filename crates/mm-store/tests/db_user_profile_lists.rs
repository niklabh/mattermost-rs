//! The five profile-listing queries and the two etags behind `GET /api/v4/users`, against a
//! real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_user_profile_lists
//! ```
//!
//! Everything here is a decision only a database can settle: which join condition excludes whom,
//! whether the `DeleteAt` predicate is on the user or the membership, and which side of an
//! anti-join the `IS NULL` belongs to. The fixture is built so that **every one of those has an
//! observable consequence** — a user who left the team, a deactivated user inside the team, a
//! deactivated user outside the channel, and a user whose `UpdateAt` is the newest in the team
//! but whose membership is gone.

use mm_store::{UserStore, user_store::SqlUserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrsulist000000000000team1";
const OTHER_TEAM: &str = "mmrsulist000000000000team2";
const CHANNEL: &str = "mmrsulist000000000000chan1";

/// Ids ascend while usernames do not: `us1` is `zed`, `us2` is `amy`. A query that lost its
/// `ORDER BY Username` would come back in primary-key order and be caught by the first assert.
const ZED: &str = "mmrsulist00000000000000us1";
const AMY: &str = "mmrsulist00000000000000us2";
/// In the team, in the channel, and **deactivated** — the row that separates a `Users.DeleteAt`
/// predicate from a `TeamMembers.DeleteAt` one.
const MID: &str = "mmrsulist00000000000000us3";
/// In the team's `TeamMembers` row with `DeleteAt != 0`: left the team. Also carries the newest
/// `UpdateAt` of anyone with a membership row, because the etag query has no `DeleteAt`
/// condition and the listing query does.
const BOB: &str = "mmrsulist00000000000000us4";
/// No membership anywhere.
const EVE: &str = "mmrsulist00000000000000us5";
/// In the team, **not** in the channel, and deactivated — proves `GetProfilesNotInChannel` has
/// no `DeleteAt` filter of any kind.
const DAN: &str = "mmrsulist00000000000000us6";

/// Year 2255 in epoch milliseconds — comfortably above anything the development database holds,
/// so `MAX(UpdateAt)` over an open-ended set is this fixture's own value and nobody else's.
const BOB_UPDATE_AT: i64 = 9_000_000_000_000;
/// One second later, for the "the MAX moved" half of the etag test.
const EVE_UPDATE_AT: i64 = 9_000_000_001_000;

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
        "DELETE FROM channelmembers WHERE userid LIKE 'mmrsulist%'",
        "DELETE FROM teammembers WHERE userid LIKE 'mmrsulist%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsulist%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsulist%'",
        "DELETE FROM users WHERE id LIKE 'mmrsulist%'",
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

async fn insert_team(pool: &PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type,
                            allowopeninvite)
         VALUES ($1, 0, 0, 0, 'mmrs ul', $2, 'O', false)",
    )
    .bind(id)
    .bind(format!("mmrs-ul-{id}"))
    .execute(pool)
    .await
    .expect("inserts the team");
}

async fn add_to_team(pool: &PgPool, team: &str, user: &str, member_delete_at: i64) {
    sqlx::query(
        "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                  schemeguest, createat)
         VALUES ($1, $2, '', $3, true, false, false, 0)",
    )
    .bind(team)
    .bind(user)
    .bind(member_delete_at)
    .execute(pool)
    .await
    .expect("inserts the team membership");
}

async fn add_to_channel(pool: &PgPool, channel: &str, user: &str) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                     mentioncount, mentioncountroot, msgcountroot,
                                     urgentmentioncount, notifyprops, lastupdateat,
                                     schemeuser, schemeadmin, schemeguest)
         VALUES ($1, $2, '', 0, 0, 0, 0, 0, 0, '{}'::jsonb, 0, true, false, false)",
    )
    .bind(channel)
    .bind(user)
    .execute(pool)
    .await
    .expect("inserts the channel membership");
}

async fn seed(pool: &PgPool) {
    purge(pool).await;

    insert_user(pool, ZED, "mmrsulist-zed", 3_000, 0).await;
    insert_user(pool, AMY, "mmrsulist-amy", 1_000, 0).await;
    insert_user(pool, MID, "mmrsulist-mid", 2_000, 9_999).await;
    // Far past every real row's `UpdateAt`: the not-in-team etag's `MAX` runs over *every*
    // user outside the team, which on a shared development database means the Go fixture users
    // too. Only a value that dominates them makes the aggregate assertable.
    insert_user(pool, BOB, "mmrsulist-bob", BOB_UPDATE_AT, 0).await;
    insert_user(pool, EVE, "mmrsulist-eve", 500, 0).await;
    insert_user(pool, DAN, "mmrsulist-dan", 1_500, 7_777).await;

    insert_team(pool, TEAM).await;
    insert_team(pool, OTHER_TEAM).await;
    for user in [ZED, AMY, MID, DAN] {
        add_to_team(pool, TEAM, user, 0).await;
    }
    // The membership is soft-deleted: still a row, and still visible to the etag query, but
    // gone from the listing.
    add_to_team(pool, TEAM, BOB, 4_242).await;
    add_to_team(pool, OTHER_TEAM, EVE, 0).await;

    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                               name, totalmsgcount, totalmsgcountroot)
         VALUES ($1, 0, 0, 0, $2, 'O'::channel_type, 'mmrs ul', $3, 0, 0)",
    )
    .bind(CHANNEL)
    .bind(TEAM)
    .bind(format!("mmrs-ul-{CHANNEL}"))
    .execute(pool)
    .await
    .expect("inserts the channel");
    for user in [ZED, MID] {
        add_to_channel(pool, CHANNEL, user).await;
    }
}

fn usernames(users: &[mm_model::user::User]) -> Vec<&str> {
    users.iter().map(|u| u.username.as_str()).collect()
}

/// The fixture's own users, in the order the query returned them, with everything else in the
/// development database filtered out.
fn ours(users: &[mm_model::user::User]) -> Vec<&str> {
    users
        .iter()
        .map(|u| u.username.as_str())
        .filter(|name| name.starts_with("mmrsulist-"))
        .collect()
}

#[tokio::test]
async fn in_team_excludes_a_left_member_and_the_active_filter_is_on_the_user_row() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let all = store
        .get_profiles_in_team(TEAM, 0, 60, None)
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&all),
        vec![
            "mmrsulist-amy",
            "mmrsulist-dan",
            "mmrsulist-mid",
            "mmrsulist-zed"
        ],
        "Username ASC, not id order; `bob` is excluded by tm.DeleteAt != 0, and the two \
         deactivated *users* are present because no DeleteAt predicate applies by default"
    );

    let live = store
        .get_profiles_in_team(TEAM, 0, 60, Some(false))
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&live),
        vec!["mmrsulist-amy", "mmrsulist-zed"],
        "active=true is Users.DeleteAt = 0"
    );

    let gone = store
        .get_profiles_in_team(TEAM, 0, 60, Some(true))
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&gone),
        vec!["mmrsulist-dan", "mmrsulist-mid"],
        "inactive=true is Users.DeleteAt != 0 — and it still does not admit `bob`, whose user \
         row is alive and whose membership is not"
    );

    let empty = store
        .get_profiles_in_team(OTHER_TEAM, 0, 60, None)
        .await
        .expect("query runs");
    assert_eq!(usernames(&empty), vec!["mmrsulist-eve"]);

    purge(&pool).await;
}

#[tokio::test]
async fn paging_is_offset_page_times_per_page_and_per_page_zero_is_an_empty_page() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let first = store
        .get_profiles_in_team(TEAM, 0, 2, None)
        .await
        .expect("query runs");
    let second = store
        .get_profiles_in_team(TEAM, 1, 2, None)
        .await
        .expect("query runs");
    let third = store
        .get_profiles_in_team(TEAM, 2, 2, None)
        .await
        .expect("query runs");
    assert_eq!(usernames(&first), vec!["mmrsulist-amy", "mmrsulist-dan"]);
    assert_eq!(
        usernames(&second),
        vec!["mmrsulist-mid", "mmrsulist-zed"],
        "OFFSET is page * per_page — swapping the two parameters gives amy/dan again"
    );
    assert!(third.is_empty(), "past the end is empty, not an error");

    // Squirrel emits `LIMIT 0` for `per_page=0` here — unlike the channel-member store, whose
    // `Limit > 0` guard turns it into "no limit". Same query parameter, opposite meaning.
    let none = store
        .get_profiles_in_team(TEAM, 0, 0, None)
        .await
        .expect("query runs");
    assert!(none.is_empty(), "per_page=0 is LIMIT 0 on this route");

    purge(&pool).await;
}

#[tokio::test]
async fn in_channel_has_no_membership_deletion_condition_and_orders_by_username() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let all = store
        .get_profiles_in_channel(CHANNEL, 0, 60, None)
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&all),
        vec!["mmrsulist-mid", "mmrsulist-zed"],
        "the two channel members, username order — `amy` is in the team but not the channel"
    );

    let live = store
        .get_profiles_in_channel(CHANNEL, 0, 60, Some(false))
        .await
        .expect("query runs");
    assert_eq!(usernames(&live), vec!["mmrsulist-zed"]);
    let gone = store
        .get_profiles_in_channel(CHANNEL, 0, 60, Some(true))
        .await
        .expect("query runs");
    assert_eq!(usernames(&gone), vec!["mmrsulist-mid"]);

    purge(&pool).await;
}

#[tokio::test]
async fn not_in_channel_is_an_anti_join_scoped_to_the_team_and_keeps_deactivated_users() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .get_profiles_not_in_channel(TEAM, CHANNEL, 0, 60)
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&users),
        vec!["mmrsulist-amy", "mmrsulist-dan"],
        "team members outside the channel. `zed`/`mid` are in it; `bob` left the team; `eve` \
         was never in it; and `dan` is deactivated but listed — this query takes no options at \
         all, so there is no DeleteAt filter to apply"
    );

    // The anti-join's two halves: the channel id belongs to the LEFT JOIN condition, so a
    // channel nobody is in returns the whole team rather than nothing.
    let unknown_channel = store
        .get_profiles_not_in_channel(TEAM, "mmrsulist000000000000nochn", 0, 60)
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&unknown_channel),
        vec![
            "mmrsulist-amy",
            "mmrsulist-dan",
            "mmrsulist-mid",
            "mmrsulist-zed"
        ]
    );

    let paged = store
        .get_profiles_not_in_channel(TEAM, CHANNEL, 1, 1)
        .await
        .expect("query runs");
    assert_eq!(
        usernames(&paged),
        vec!["mmrsulist-dan"],
        "the caller passes an offset here, already multiplied"
    );

    purge(&pool).await;
}

#[tokio::test]
async fn not_in_team_lists_the_left_member_and_never_the_current_ones() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let users = store
        .get_profiles_not_in_team(TEAM, 0, 200)
        .await
        .expect("query runs");
    assert_eq!(
        ours(&users),
        vec!["mmrsulist-bob", "mmrsulist-eve"],
        "`bob`'s membership is soft-deleted, so the same `tm.DeleteAt = 0` that *excludes* him \
         from the in-team listing *includes* him here"
    );

    purge(&pool).await;
}

#[tokio::test]
async fn all_profiles_is_username_ordered_and_carries_the_active_filter() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());

    let all = store
        .get_all_profiles(0, 200, None)
        .await
        .expect("query runs");
    assert!(
        all.len() < 200,
        "the development database has grown past one page; this assertion needs a smaller \
         database or a keyset walk ({} users)",
        all.len()
    );
    assert_eq!(
        ours(&all),
        vec![
            "mmrsulist-amy",
            "mmrsulist-bob",
            "mmrsulist-dan",
            "mmrsulist-eve",
            "mmrsulist-mid",
            "mmrsulist-zed"
        ],
        "no filter at all: memberships are irrelevant and deactivated users are included"
    );
    let names = usernames(&all);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "ORDER BY Users.Username ASC");

    let gone = store
        .get_all_profiles(0, 200, Some(true))
        .await
        .expect("query runs");
    assert_eq!(ours(&gone), vec!["mmrsulist-dan", "mmrsulist-mid"]);

    let live = store
        .get_all_profiles(0, 200, Some(false))
        .await
        .expect("query runs");
    assert_eq!(
        ours(&live),
        vec![
            "mmrsulist-amy",
            "mmrsulist-bob",
            "mmrsulist-eve",
            "mmrsulist-zed"
        ]
    );

    purge(&pool).await;
}

#[tokio::test]
async fn the_in_team_etag_has_no_membership_deletion_condition() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());
    let version = mm_model::utils::CURRENT_VERSION;

    // `bob` has the newest UpdateAt (8000) and a soft-deleted membership. The listing hides
    // him; the etag query, which joins `Users, TeamMembers` with no DeleteAt condition, does
    // not — so the etag moves when a *former* member is edited.
    assert_eq!(
        store.get_etag_for_profiles(TEAM).await,
        format!("{version}.{BOB_UPDATE_AT}"),
        "MAX over every membership row, deleted or not — and DESC, not ASC"
    );

    assert_eq!(
        store.get_etag_for_profiles(OTHER_TEAM).await,
        format!("{version}.500")
    );

    // A team with no members at all has no row to read, and Go falls back to the clock — so
    // the etag is deliberately different every time and can never produce a 304.
    let first = store
        .get_etag_for_profiles("mmrsulist000000000000notem")
        .await;
    let second = store
        .get_etag_for_profiles("mmrsulist000000000000notem")
        .await;
    assert!(first.starts_with(&format!("{version}.")));
    assert_ne!(
        first, second,
        "the millisecond fallback must not be mistaken for a stable etag"
    );

    purge(&pool).await;
}

#[tokio::test]
async fn the_not_in_team_etag_is_max_dot_count_and_moves_with_both() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlUserStore::new(pool.clone());
    let version = mm_model::utils::CURRENT_VERSION;

    let baseline = store.get_etag_for_profiles_not_in_team(TEAM).await;
    assert!(baseline.starts_with(&format!("{version}.")), "{baseline}");
    assert_eq!(
        baseline,
        store.get_etag_for_profiles_not_in_team(TEAM).await,
        "stable — this one never reaches the clock, because the aggregate always returns a row"
    );
    // `bob`'s membership is soft-deleted, so he counts as not-in-team and sets the MAX.
    let tail = baseline
        .strip_prefix(&format!("{version}."))
        .expect("the version prefix");
    let (max, count) = tail.split_once('.').expect("MAX.COUNT");
    assert_eq!(
        max,
        BOB_UPDATE_AT.to_string(),
        "the newest UpdateAt among non-members"
    );
    let count: i64 = count.parse().expect("a count");
    assert!(count >= 2, "at least bob and eve: {baseline}");

    // COUNT moves when a non-member appears...
    insert_user(&pool, "mmrsulist00000000000000us7", "mmrsulist-new", 100, 0).await;
    let after_insert = store.get_etag_for_profiles_not_in_team(TEAM).await;
    assert_ne!(after_insert, baseline, "COUNT(Id) is part of the etag");
    let after_count: i64 = after_insert
        .rsplit_once('.')
        .and_then(|(_, n)| n.parse().ok())
        .expect("a count");
    assert!(
        after_count > count,
        "{after_insert} vs {baseline} — the new user's UpdateAt (100) is below the MAX, so only          COUNT can have moved"
    );

    // ...and MAX moves when a non-member is touched.
    sqlx::query("UPDATE users SET updateat = $2 WHERE id = $1")
        .bind(EVE)
        .bind(EVE_UPDATE_AT)
        .execute(&pool)
        .await
        .expect("updates eve");
    let after_update = store.get_etag_for_profiles_not_in_team(TEAM).await;
    assert!(
        after_update.starts_with(&format!("{version}.{EVE_UPDATE_AT}.")),
        "MAX(UpdateAt) is part of the etag: {after_update}"
    );

    // A team every user belongs to would give the empty aggregate; the closest reachable case
    // is the literal shape, which is `.0` and never the clock.
    let empty = store
        .get_etag_for_profiles_not_in_team("mmrsulist000000000000notem")
        .await;
    assert!(
        !empty.ends_with(".0"),
        "the development database always has users outside a nonexistent team: {empty}"
    );

    purge(&pool).await;
}
