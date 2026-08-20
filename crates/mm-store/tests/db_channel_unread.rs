//! `SqlChannelStore::get_channel_unread` against a real Postgres, on fixtures the REST API
//! cannot build.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_unread
//! ```
//!
//! # Why this exists next to a cross-server parity suite that already covers the route
//!
//! `crates/mm-api/tests/parity_channel_unread.rs` compares both servers over HTTP and catches
//! almost everything. Two of the query's four `WHERE` predicates are invisible to it:
//!
//! - **`Channels.Type IN (O, P, D, G)`.** A board (`BO`/`BP`) or space (`S`) cannot be created
//!   through the REST API on Team Edition, and even if it could the route's permission check
//!   calls `SqlChannelStore::Get` first — which applies the *same* filter, misses, and denies with
//!   a 403 before this query ever runs. So over HTTP the predicate is unreachable, and deleting it
//!   passed the entire parity suite. It is reachable here.
//! - **`DeleteAt = 0` resolving to the *channel's* column.** The parity suite does cover this
//!   (archiving a channel is a REST call), but only for the archived case. The row below pins the
//!   asymmetry directly: the membership survives, the unread state does not.
//!
//! # This one is transcribed, not measured, and that is a real weakening
//!
//! Every other claim about this query is checked against the running Go server. These two are
//! read off `channel_store.go:921-937` and asserted against our own implementation, because Go's
//! `SqlChannelStore` is not reachable from a test and the route in front of it cannot express the
//! input. If upstream widens `messageChannelTypes`, this test keeps passing while the port drifts.
//! Recorded as such rather than dressed up as an oracle.
//!
//! # Every row here is `mmrs`-prefixed and removed before and after

use mm_store::channel_store::get_channel_unread;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const TEAM: &str = "mmrsunreadteam00000000team";
const USER: &str = "mmrsunreaduser00000000user";
const OPEN_CHANNEL: &str = "mmrsunreadchan000000000pen";
const BOARD_CHANNEL: &str = "mmrsunreadchan00000000bord";
const ARCHIVED_CHANNEL: &str = "mmrsunreadchan00000000arch";

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsunread%' OR userid LIKE 'mmrsunread%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsunread%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsunread%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// One team, three channels differing **only** in `Type` and `DeleteAt`, and one membership row in
/// each with identical counters.
///
/// Identical counters are the point: any difference in what the three reads return comes from the
/// predicate under test and from nothing else.
async fn seed(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
         VALUES ($1, 0, 0, 0, 'mmrs unread', 'mmrs-unread-team', 'O', false)",
    )
    .bind(TEAM)
    .execute(pool)
    .await
    .expect("inserts the team");

    for (id, channel_type, delete_at) in [
        (OPEN_CHANNEL, "O", 0_i64),
        (BOARD_CHANNEL, "BO", 0),
        (ARCHIVED_CHANNEL, "O", 1_700_000_000_000),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, 'mmrs unread', $5, 40, 30)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(TEAM)
        .bind(channel_type)
        .bind(format!("mmrs-unread-{id}"))
        .execute(pool)
        .await
        .expect("inserts the channel");

        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                         mentioncount, mentioncountroot, msgcountroot,
                                         urgentmentioncount, notifyprops, lastupdateat,
                                         schemeuser, schemeadmin, schemeguest)
             VALUES ($1, $2, 'channel_user', 0, 15, 7, 5, 12, 3, '{}'::jsonb, 0, true, false, false)",
        )
        .bind(id)
        .bind(USER)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
}

/// The arithmetic, on the one channel that qualifies: `40 - 15` and `30 - 12`, with the three
/// mention counters passed through unchanged.
///
/// Asserting the mention counters as three *different* numbers is deliberate — 7, 5 and 3 cannot
/// be permuted without this failing, where three equal values could.
#[tokio::test]
async fn the_counters_are_the_channel_total_minus_the_members_own() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let unread = get_channel_unread(&pool, OPEN_CHANNEL, USER)
        .await
        .expect("the open channel has unread state");

    purge(&pool).await;

    assert_eq!(unread.team_id, TEAM);
    assert_eq!(unread.channel_id, OPEN_CHANNEL);
    assert_eq!(unread.msg_count, 25, "40 - 15");
    assert_eq!(unread.msg_count_root, 18, "30 - 12");
    assert_eq!(unread.mention_count, 7);
    assert_eq!(unread.mention_count_root, 5);
    assert_eq!(unread.urgent_mention_count, 3);
}

/// **A board is invisible here even with a membership row and unread traffic.**
///
/// `messageChannelTypes` (channel_store.go:38) is the allow-list, and this is the only place the
/// port can be asked about it: the route in front of this query refuses a board at its permission
/// check, so deleting the predicate passes every cross-server test. Transcribed from Go's SQL —
/// see the module docs for why that is weaker than the rest of this suite.
#[tokio::test]
async fn a_board_channel_has_no_unread_state() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let result = get_channel_unread(&pool, BOARD_CHANNEL, USER).await;
    let open = get_channel_unread(&pool, OPEN_CHANNEL, USER).await;

    purge(&pool).await;

    let err = result.expect_err("a BO channel is not a message channel");
    assert!(
        err.is_not_found(),
        "the type filter must miss, not fail: {err}"
    );
    // The control: the *same* fixture shape at type `O` is found, so the miss above is the type
    // and not something else about the row.
    assert!(
        open.is_ok(),
        "the identical row at type O must be found, or the test above proves nothing"
    );
}

/// **An archived channel has no unread state, while its membership row is still readable.**
///
/// The bare `DeleteAt = 0` in Go's query resolves to `Channels.DeleteAt`, because
/// `ChannelMembers` has no such column. `GetMember` takes an `includeDeleted` flag instead and the
/// api4 call site passes `true`, so the same two ids answer differently on the two routes.
#[tokio::test]
async fn an_archived_channel_has_no_unread_state_but_keeps_its_member() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let unread = get_channel_unread(&pool, ARCHIVED_CHANNEL, USER).await;
    let member = mm_store::channel_store::get_member(&pool, ARCHIVED_CHANNEL, USER).await;

    purge(&pool).await;

    let err = unread.expect_err("an archived channel has no unread state");
    assert!(err.is_not_found(), "{err}");
    assert!(
        member.is_ok(),
        "GetMember does not filter DeleteAt — that asymmetry is the finding"
    );
}

/// A user with no membership row is a miss, not a row of zeroes. The implicit join is what makes
/// that true, and a LEFT join here would invent unread state for a non-member.
#[tokio::test]
async fn a_non_member_is_a_miss() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let result = get_channel_unread(&pool, OPEN_CHANNEL, "mmrsunreadnobody000000000x").await;

    purge(&pool).await;

    let err = result.expect_err("a non-member has no unread state");
    assert!(err.is_not_found(), "{err}");
}
