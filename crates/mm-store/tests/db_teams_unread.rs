//! `SqlTeamStore::get_channel_unreads_for_all_teams` against a real Postgres, on the rows the
//! REST API cannot build.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_teams_unread
//! ```
//!
//! `crates/mm-api/tests/parity_teams_unread.rs` measures the exclusion predicate, the archived
//! channel and the fold against the running Go server. Two things it cannot reach:
//!
//! - **The type deny-list is `NOT IN ('S')`, not `GetChannelUnread`'s `IN (O, P, D, G)`.** A
//!   board's counters feed the team badge here and are refused one channel at a time there. No
//!   board or space exists on Team Edition over REST, so both rows are planted directly. This is
//!   transcribed from team_store.go:1239 and asserted against our own port — a weaker claim than
//!   the parity suite's, recorded as such.
//! - **NULL counters are a 500, not a zero.** Nothing in Go's query coalesces, and nothing in the
//!   REST API writes a NULL `MentionCount`; the row is written here to pin that the port does not
//!   quietly `COALESCE` where Go would fail the scan.
//!
//! Every row is `mmrstu`-prefixed and removed before and after.

use mm_store::team_store::get_channel_unreads_for_all_teams;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The tests share fixture rows and each purges before seeding; serialised so they do not delete
/// each other's rows mid-assertion.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM_A: &str = "mmrstuteam0000000000000aaa";
const TEAM_B: &str = "mmrstuteam0000000000000bbb";
const USER: &str = "mmrstuuser000000000000user";
const OPEN_A: &str = "mmrstuchan00000000000opena";
const BOARD_A: &str = "mmrstuchan00000000000board";
const SPACE_A: &str = "mmrstuchan00000000000space";
const ARCHIVED_A: &str = "mmrstuchan000000000archivd";
const OPEN_B: &str = "mmrstuchan00000000000openb";
const DIRECT: &str = "mmrstuchan0000000000direct";
const NULLED_A: &str = "mmrstuchan00000000000nulld";

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrstu%' OR userid LIKE 'mmrstu%'",
        "DELETE FROM channels WHERE id LIKE 'mmrstu%'",
        "DELETE FROM teams WHERE id LIKE 'mmrstu%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Two teams plus a team-less direct channel. Every channel carries a **distinct** counter set
/// so a per-channel omission or double count shows up as a specific wrong sum, not a coincidence.
async fn seed(pool: &PgPool, include_nulled: bool) {
    for team in [TEAM_A, TEAM_B] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs tu', $2, 'O', false)",
        )
        .bind(team)
        .bind(format!("mmrs-tu-{team}"))
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    // (id, team, type, delete_at, total, total_root, msg, msg_root, mention, mention_root)
    let mut channels = vec![
        (
            OPEN_A, TEAM_A, "O", 0_i64, 40_i64, 30_i64, 15_i64, 12_i64, 7_i64, 5_i64,
        ),
        (BOARD_A, TEAM_A, "BO", 0, 400, 300, 150, 120, 70, 50),
        (SPACE_A, TEAM_A, "S", 0, 4000, 3000, 1500, 1200, 700, 500),
        (ARCHIVED_A, TEAM_A, "O", 1_700_000_000_000, 9, 8, 1, 1, 1, 1),
        (OPEN_B, TEAM_B, "O", 0, 20, 10, 2, 1, 3, 2),
        (DIRECT, "", "D", 0, 60, 50, 10, 10, 4, 3),
    ];
    if include_nulled {
        channels.push((NULLED_A, TEAM_A, "O", 0, 5, 5, 1, 1, 1, 1));
    }

    for (
        id,
        team,
        channel_type,
        delete_at,
        total,
        total_root,
        msg,
        msg_root,
        mention,
        mention_root,
    ) in channels
    {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, 'mmrs tu', $5, $6, $7)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(team)
        .bind(channel_type)
        .bind(format!("mmrs-tu-{id}"))
        .bind(total)
        .bind(total_root)
        .execute(pool)
        .await
        .expect("inserts the channel");

        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                         mentioncount, mentioncountroot, msgcountroot,
                                         urgentmentioncount, notifyprops, lastupdateat,
                                         schemeuser, schemeadmin, schemeguest)
             VALUES ($1, $2, 'channel_user', 0, $3, $4, $5, $6, 0, '{}'::jsonb, 0, true, false, false)",
        )
        .bind(id)
        .bind(USER)
        .bind(msg)
        .bind(mention)
        .bind(mention_root)
        .bind(msg_root)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }

    if include_nulled {
        sqlx::query("UPDATE channelmembers SET mentioncount = NULL WHERE channelid = $1")
            .bind(NULLED_A)
            .execute(pool)
            .await
            .expect("nulls the counter");
    }
}

fn by_channel(
    rows: &[mm_model::channel_member::ChannelUnread],
    channel: &str,
) -> Option<mm_model::channel_member::ChannelUnread> {
    rows.iter().find(|r| r.channel_id == channel).cloned()
}

/// With no exclusion: the board **is** returned (the deny-list is spaces only), the space and the
/// archived channel are not, and the direct channel is hidden by `TeamId <> ''`.
#[tokio::test]
async fn the_deny_list_is_spaces_only_and_an_empty_exclusion_hides_direct_channels() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, false).await;

    let rows = get_channel_unreads_for_all_teams(&pool, "", USER)
        .await
        .expect("the query runs");

    purge(&pool).await;

    let mut ids: Vec<&str> = rows.iter().map(|r| r.channel_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![BOARD_A, OPEN_A, OPEN_B],
        "board in (NOT IN ('S') is narrower than IN (O,P,D,G)); space, archived and the \
         team-less DM out"
    );

    let open_a = by_channel(&rows, OPEN_A).expect("open A");
    assert_eq!(open_a.team_id, TEAM_A);
    assert_eq!(open_a.msg_count, 25, "40 - 15");
    assert_eq!(open_a.msg_count_root, 18, "30 - 12");
    assert_eq!(open_a.mention_count, 7);
    assert_eq!(open_a.mention_count_root, 5);
    assert_eq!(
        open_a.urgent_mention_count, 0,
        "not selected by Go's query; stays at the zero value"
    );
    assert!(open_a.notify_props.as_ref().is_some_and(|p| p.is_empty()));

    let board = by_channel(&rows, BOARD_A).expect("board A");
    assert_eq!(board.msg_count, 250);
    assert_eq!(board.mention_count_root, 50);
}

/// Excluding team A drops its rows and **admits the direct channel** with an empty `team_id` —
/// the consequence of Go's unconditional `TeamId <> ?` that the sibling `GetTeamsForUser`'s
/// conditional form would not have.
#[tokio::test]
async fn excluding_a_team_drops_it_and_admits_the_team_less_direct_channel() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, false).await;

    let rows = get_channel_unreads_for_all_teams(&pool, TEAM_A, USER)
        .await
        .expect("the query runs");

    purge(&pool).await;

    let mut ids: Vec<&str> = rows.iter().map(|r| r.channel_id.as_str()).collect();
    ids.sort_unstable();
    let mut expected = vec![DIRECT, OPEN_B];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    let direct = by_channel(&rows, DIRECT).expect("the DM");
    assert_eq!(
        direct.team_id, "",
        "a DM's TeamId is the empty string, and it is on the wire"
    );
    assert_eq!(direct.msg_count, 50, "60 - 10");
    assert_eq!(direct.mention_count, 4);
}

/// A NULL counter fails the read, as Go's `int64` scan does — no `COALESCE` anywhere in this
/// query. The control is the same fixture without the nulled row, which succeeds above.
#[tokio::test]
async fn a_null_counter_is_an_error_not_a_zero() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool, true).await;

    let result = get_channel_unreads_for_all_teams(&pool, "", USER).await;

    purge(&pool).await;

    let err = result.expect_err("a NULL MentionCount cannot scan into int64");
    assert!(!err.is_not_found(), "a scan failure, not a miss: {err}");
}
