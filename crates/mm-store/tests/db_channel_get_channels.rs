//! `SqlChannelStore::get_channels` and `get_by_name` against a real Postgres, on the branches
//! the REST routes in front of them never reach.
//!
//! ```sh
//! scripts/parity.sh -p mm-store --test db_channel_get_channels
//! ```
//!
//! # What only this file can pin
//!
//! `crates/mm-api/tests/parity_channels_for_team_for_user.rs` covers the deletion filters, the
//! display-name order, the DM-in-every-team rule and the empty-result 404 against the running
//! Go server. Not reachable over HTTP, and transcribed from `SqlChannelStore.GetChannels`
//! (channel_store.go:1208) rather than measured — [D-151]'s shape:
//!
//! - **`LastUpdateAt > 0`** adds `UpdateAt >= ?`; `getChannelsForTeamForUser` never sets it.
//! - **`Type IN (O, P, D, G)`** — a board (`BO`) cannot be created on Team Edition.
//! - **The empty-team wildcard** — Go omits the team predicate for `teamId == ""`; the handler
//!   validates the id first, so no request arrives with it empty.
//!
//! And for `getByName` (channel_store.go:1684), the rule the by-name suite shows only from the
//! DM side: an **empty** team id finds **only** teamless channels, because the predicate is a
//! literal `TeamId = '' OR TeamId = ''` rather than `getByNames`'s omitted one.

use mm_model::channel::ChannelSearchOpts;
use mm_store::channel_store::{get_by_name, get_channels};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsgetchanuser00000000001";
const TEAM_A: &str = "mmrsgetchanteam0000000aaaa";
const TEAM_B: &str = "mmrsgetchanteam0000000bbbb";

const A_OPEN_OLD: &str = "mmrsgetchanchan00000aopen1"; // team A, O, display "b old", updateat 100
const A_OPEN_NEW: &str = "mmrsgetchanchan00000aopen2"; // team A, O, display "a new", updateat 300
const A_PRIVATE: &str = "mmrsgetchanchan00000apriv1"; // team A, P, display "c priv", updateat 200
const A_BOARD: &str = "mmrsgetchanchan00000aboard"; // team A, BO
const A_NOT_MEMBER: &str = "mmrsgetchanchan00000anomem"; // team A, O, no membership row
const DM: &str = "mmrsgetchanchan00000dm0001"; // team "", D, display "" (sorts first)
const B_OPEN: &str = "mmrsgetchanchan00000bopen1"; // team B, O

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsgetchan%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsgetchan%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsgetchan%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn seed(pool: &PgPool) {
    for (team, name) in [(TEAM_A, "mmrs-getchan-a"), (TEAM_B, "mmrs-getchan-b")] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs getchan', $2, 'O', false)",
        )
        .bind(team)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    for (id, team, channel_type, display_name, name, update_at, member) in [
        (
            A_OPEN_OLD,
            TEAM_A,
            "O",
            "b old",
            "mmrs-getchan-old",
            100_i64,
            true,
        ),
        (
            A_OPEN_NEW,
            TEAM_A,
            "O",
            "a new",
            "mmrs-getchan-new",
            300,
            true,
        ),
        (
            A_PRIVATE,
            TEAM_A,
            "P",
            "c priv",
            "mmrs-getchan-priv",
            200,
            true,
        ),
        (
            A_BOARD,
            TEAM_A,
            "BO",
            "d board",
            "mmrs-getchan-board",
            300,
            true,
        ),
        (
            A_NOT_MEMBER,
            TEAM_A,
            "O",
            "e nomem",
            "mmrs-getchan-nomem",
            300,
            false,
        ),
        (DM, "", "D", "", "mmrs-getchan-dm", 300, true),
        (
            B_OPEN,
            TEAM_B,
            "O",
            "f other team",
            "mmrs-getchan-b",
            300,
            true,
        ),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, $2, 0, $3, $4::channel_type, $5, $6, 0, 0)",
        )
        .bind(id)
        .bind(update_at)
        .bind(team)
        .bind(channel_type)
        .bind(display_name)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the channel");

        if member {
            sqlx::query(
                "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser)
                 VALUES ($1, $2, '', '{}'::jsonb, true)",
            )
            .bind(id)
            .bind(USER)
            .execute(pool)
            .await
            .expect("inserts the membership");
        }
    }
}

fn ids(list: &mm_model::channel_list::ChannelList) -> Vec<&str> {
    list.0.iter().map(|c| c.id.as_str()).collect()
}

/// The baseline: team A's member channels plus the teamless DM, in display-name order — which
/// puts the DM (empty display name) first and the newest-created channel second, so heap order
/// cannot pass for `ORDER BY`. The board and the non-member channel are absent.
#[tokio::test]
async fn a_team_lists_member_message_channels_and_dms_by_display_name() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let list = get_channels(&pool, TEAM_A, USER, &ChannelSearchOpts::default())
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(ids(&list), vec![DM, A_OPEN_NEW, A_OPEN_OLD, A_PRIVATE]);
}

/// `LastUpdateAt > 0` keeps only channels updated at or after it — `>=`, so the boundary row
/// stays. Zero and negative values add no predicate.
#[tokio::test]
async fn last_update_at_filters_with_greater_or_equal() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let at_200 = get_channels(
        &pool,
        TEAM_A,
        USER,
        &ChannelSearchOpts {
            last_update_at: 200,
            ..Default::default()
        },
    )
    .await
    .expect("queries");
    let at_201 = get_channels(
        &pool,
        TEAM_A,
        USER,
        &ChannelSearchOpts {
            last_update_at: 201,
            ..Default::default()
        },
    )
    .await
    .expect("queries");
    let negative = get_channels(
        &pool,
        TEAM_A,
        USER,
        &ChannelSearchOpts {
            last_update_at: -5,
            ..Default::default()
        },
    )
    .await
    .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(&at_200),
        vec![DM, A_OPEN_NEW, A_PRIVATE],
        ">= keeps 200"
    );
    assert_eq!(ids(&at_201), vec![DM, A_OPEN_NEW], "201 drops it");
    assert_eq!(ids(&negative).len(), 4, "a negative value is no filter");
}

/// An empty team id omits the team predicate — every team's member channels come back — and a
/// member of nothing is `NotFound`, not an empty list.
#[tokio::test]
async fn an_empty_team_searches_every_team_and_no_rows_is_not_found() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let everywhere = get_channels(&pool, "", USER, &ChannelSearchOpts::default())
        .await
        .expect("queries");
    let nobody = get_channels(
        &pool,
        TEAM_A,
        "mmrsgetchanuser00000000002",
        &ChannelSearchOpts::default(),
    )
    .await;

    purge(&pool).await;
    assert_eq!(
        ids(&everywhere),
        vec![DM, A_OPEN_NEW, A_OPEN_OLD, A_PRIVATE, B_OPEN]
    );
    assert!(
        nobody.expect_err("zero rows").is_not_found(),
        "Go returns ErrNotFound for an empty result"
    );
}

/// `getByName`'s team rule: a real team id finds its own channel **and** a teamless one; an
/// empty team id finds **only** teamless channels; the board is invisible under any team.
#[tokio::test]
async fn get_by_name_admits_teamless_channels_under_any_team_and_nothing_else_under_none() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let own = get_by_name(&pool, TEAM_A, "mmrs-getchan-old", false).await;
    let dm_under_a = get_by_name(&pool, TEAM_A, "mmrs-getchan-dm", false).await;
    let dm_under_b = get_by_name(&pool, TEAM_B, "mmrs-getchan-dm", false).await;
    let other_team = get_by_name(&pool, TEAM_B, "mmrs-getchan-old", false).await;
    let teamless_only = get_by_name(&pool, "", "mmrs-getchan-old", false).await;
    let teamless_dm = get_by_name(&pool, "", "mmrs-getchan-dm", false).await;
    let board = get_by_name(&pool, TEAM_A, "mmrs-getchan-board", false).await;

    purge(&pool).await;
    assert_eq!(own.expect("found").id, A_OPEN_OLD);
    assert_eq!(dm_under_a.expect("found").id, DM);
    assert_eq!(dm_under_b.expect("found").id, DM);
    assert!(other_team.expect_err("wrong team").is_not_found());
    assert!(
        teamless_only
            .expect_err("an empty team id is not a wildcard here")
            .is_not_found()
    );
    assert_eq!(teamless_dm.expect("found").id, DM);
    assert!(
        board
            .expect_err("BO is not a message channel")
            .is_not_found()
    );
}
