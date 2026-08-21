//! `SqlChannelStore::get_channels_by_user` against a real Postgres, on the branches the REST
//! route in front of it never reaches.
//!
//! ```sh
//! scripts/parity.sh -p mm-store --test db_channel_get_channels_by_user
//! ```
//!
//! # What only this file can pin
//!
//! `crates/mm-api/tests/parity_channels_for_user.rs` covers the id order, the page loop at 100,
//! the deletion filters on channel and team, and the empty-result `not_found` against the
//! running Go server. Not reachable over HTTP, and transcribed from
//! `SqlChannelStore.GetChannelsByUser` (channel_store.go:1264) rather than measured:
//!
//! - **`page_size = -1`** is "no `LIMIT`"; the handler always passes 100.
//! - **The keyset is strict** (`Id > from`): the row the previous page ended on is not repeated.
//!   A page of one pins it, which the route's 100 cannot.
//! - **`Type IN (O, P, D, G)`** — a board (`BO`) cannot be created on Team Edition.
//! - **`include_deleted` with `last_delete_at = 0` is no filter at all** — an archived channel of
//!   an archived team comes back — whereas the REST suite's archived fixtures are always in a
//!   living team or living channels in an archived one, never both archived.

use mm_store::channel_store::get_channels_by_user;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsbyuseruser000000000001";
const STRANGER: &str = "mmrsbyuseruser000000000002";
const TEAM_LIVE: &str = "mmrsbyuserteam00000000live";
const TEAM_DEAD: &str = "mmrsbyuserteam00000000dead"; // deleteat 500

// Ids chosen so id order differs from insertion order and from display-name order.
const C1_LIVE_OPEN: &str = "mmrsbyuserchan00000000000a"; // live team, O, live
const C2_LIVE_PRIVATE: &str = "mmrsbyuserchan00000000000b"; // live team, P, live
const C3_LIVE_ARCHIVED: &str = "mmrsbyuserchan00000000000c"; // live team, O, deleteat 300
const C4_DEAD_LIVE: &str = "mmrsbyuserchan00000000000d"; // dead team, O, live
const C5_DEAD_ARCHIVED: &str = "mmrsbyuserchan00000000000e"; // dead team, O, deleteat 700
const C6_DM: &str = "mmrsbyuserchan00000000000f"; // no team, D
const C7_BOARD: &str = "mmrsbyuserchan00000000000g"; // live team, BO
const C8_NOT_MEMBER: &str = "mmrsbyuserchan00000000000h"; // live team, O, no membership

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsbyuser%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsbyuser%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsbyuser%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn seed(pool: &PgPool) {
    for (team, name, delete_at) in [
        (TEAM_LIVE, "mmrs-byuser-live", 0_i64),
        (TEAM_DEAD, "mmrs-byuser-dead", 500),
    ] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, $2, 'mmrs byuser', $3, 'O', false)",
        )
        .bind(team)
        .bind(delete_at)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    for (id, team, channel_type, name, delete_at, member) in [
        (C1_LIVE_OPEN, TEAM_LIVE, "O", "mmrs-byuser-a", 0_i64, true),
        (C2_LIVE_PRIVATE, TEAM_LIVE, "P", "mmrs-byuser-b", 0, true),
        (C3_LIVE_ARCHIVED, TEAM_LIVE, "O", "mmrs-byuser-c", 300, true),
        (C4_DEAD_LIVE, TEAM_DEAD, "O", "mmrs-byuser-d", 0, true),
        (C5_DEAD_ARCHIVED, TEAM_DEAD, "O", "mmrs-byuser-e", 700, true),
        (C6_DM, "", "D", "mmrs-byuser-f", 0, true),
        (C7_BOARD, TEAM_LIVE, "BO", "mmrs-byuser-g", 0, true),
        (C8_NOT_MEMBER, TEAM_LIVE, "O", "mmrs-byuser-h", 0, false),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, 'z', $5, 0, 0)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(team)
        .bind(channel_type)
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

/// No limit, no keyset, no deleted: the live member channels of live teams plus the DM, in id
/// order; the board, the non-member channel, the archived channel and the archived team's
/// channels are all absent.
#[tokio::test]
async fn unpaged_default_lists_live_member_message_channels_in_id_order() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let list = get_channels_by_user(&pool, USER, false, 0, -1, "")
        .await
        .expect("queries");
    let stranger = get_channels_by_user(&pool, STRANGER, false, 0, -1, "").await;

    purge(&pool).await;
    assert_eq!(ids(&list), vec![C1_LIVE_OPEN, C2_LIVE_PRIVATE, C6_DM]);
    assert!(
        stranger.expect_err("zero rows").is_not_found(),
        "Go returns ErrNotFound for an empty result"
    );
}

/// Pages of one: each starts strictly after the id it was given, and the page past the end is
/// `NotFound` — the signal the handler's loop stops on.
#[tokio::test]
async fn the_keyset_is_strict_and_the_page_past_the_end_is_not_found() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let mut walked = Vec::new();
    let mut from = String::new();
    let ended_on = loop {
        // Bounded: an inclusive keyset (`>=`) would hand back the same row forever, and a
        // hanging test is not a caught mutant.
        assert!(
            walked.len() < 10,
            "the keyset never advanced past {walked:?}"
        );
        match get_channels_by_user(&pool, USER, false, 0, 1, &from).await {
            Ok(page) => {
                assert_eq!(page.0.len(), 1, "LIMIT 1");
                from = page.0[0].id.clone();
                walked.push(from.clone());
            }
            Err(err) => break err,
        }
    };

    purge(&pool).await;
    assert_eq!(walked, vec![C1_LIVE_OPEN, C2_LIVE_PRIVATE, C6_DM]);
    assert!(ended_on.is_not_found());
}

/// `include_deleted` alone is no filter: every member message channel, archived or not, in a
/// living or archived team. With `last_delete_at`, a channel survives only if it **and** its
/// team are living or archived at-or-after the instant (`>=` on both).
#[tokio::test]
async fn include_deleted_is_unfiltered_at_zero_and_tests_channel_and_team_otherwise() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let everything = get_channels_by_user(&pool, USER, true, 0, -1, "")
        .await
        .expect("queries");
    let since_300 = get_channels_by_user(&pool, USER, true, 300, -1, "")
        .await
        .expect("queries");
    let since_301 = get_channels_by_user(&pool, USER, true, 301, -1, "")
        .await
        .expect("queries");
    let since_500 = get_channels_by_user(&pool, USER, true, 500, -1, "")
        .await
        .expect("queries");
    let since_501 = get_channels_by_user(&pool, USER, true, 501, -1, "")
        .await
        .expect("queries");
    let since_701 = get_channels_by_user(&pool, USER, true, 701, -1, "")
        .await
        .expect("queries");
    // Without include_deleted the instant is ignored.
    let ignored = get_channels_by_user(&pool, USER, false, 1, -1, "")
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(&everything),
        vec![
            C1_LIVE_OPEN,
            C2_LIVE_PRIVATE,
            C3_LIVE_ARCHIVED,
            C4_DEAD_LIVE,
            C5_DEAD_ARCHIVED,
            C6_DM
        ]
    );
    assert_eq!(
        ids(&since_300),
        ids(&everything),
        "300 keeps the channel archived at 300 and the team archived at 500"
    );
    assert_eq!(
        ids(&since_301),
        vec![
            C1_LIVE_OPEN,
            C2_LIVE_PRIVATE,
            C4_DEAD_LIVE,
            C5_DEAD_ARCHIVED,
            C6_DM
        ],
        "301 drops only the channel archived at 300"
    );
    assert_eq!(
        ids(&since_500),
        ids(&since_301),
        "500 keeps the team at 500"
    );
    assert_eq!(
        ids(&since_501),
        vec![C1_LIVE_OPEN, C2_LIVE_PRIVATE, C6_DM],
        "501 drops the archived team whole, its living channel included"
    );
    assert_eq!(
        ids(&since_701),
        ids(&since_501),
        "the channel at 700 is already gone with its team"
    );
    assert_eq!(ids(&ignored), vec![C1_LIVE_OPEN, C2_LIVE_PRIVATE, C6_DM]);
}
