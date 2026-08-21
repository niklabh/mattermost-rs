//! `SqlChannelStore::get_public_channels_for_team`, `get_private_channels_for_team` and
//! `get_deleted` against a real Postgres, on the branches the three REST routes in front of them
//! cannot reach.
//!
//! ```sh
//! scripts/parity.sh -p mm-store --test db_channel_team_lists
//! ```
//!
//! # What only this file can pin
//!
//! `crates/mm-api/tests/parity_team_channel_lists.rs` measures the visible half against the
//! running Go server: which channels each route lists, the offset paging, the empty page, the
//! three permission gates. What it cannot reach, and what is seeded directly here:
//!
//! - **`PublicChannels` drift.** Go's browse list joins a *denormalised shadow* of five channel
//!   columns and reads the team, the deletion flag and the sort key off **it**. Through the REST
//!   API the shadow always agrees with `Channels`, so `pc.TeamId` and `Channels.TeamId` are
//!   indistinguishable and every one of those three mutations survives. Rows planted here
//!   disagree on purpose, which is the only way to show which table the query believes.
//! - **A public channel with no shadow row is invisible**, which is what makes the join a type
//!   filter — Go writes no `Type = 'O'` predicate at all.
//! - **The non-message types.** A board (`BO`) cannot be created on Team Edition over REST, so
//!   `Type = 'P'` (private list) and `Type IN (O, P, D, G)` (deleted list) are transcribed from
//!   Go's SQL rather than measured — [D-151]'s shape, and said so here rather than left implied.
//! - **Archived DMs and GMs.** `GetDeleted`'s membership narrowing admits `O` and `P` only, so a
//!   `D` or `G` row is listed for a `manage_system` caller and hidden from everyone else. No
//!   archived DM exists in the shared database and nothing in the REST API makes one.

use mm_store::channel_store::{
    get_deleted, get_private_channels_for_team, get_public_channels_for_team,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsteamlistuser0000000001";
const TEAM: &str = "mmrsteamlistteam0000000001";
const OTHER_TEAM: &str = "mmrsteamlistteam0000000002";

// Living public channels of TEAM. Display names are chosen so that creation order, id order and
// display-name order all disagree.
const PUB_B: &str = "mmrsteamlistchan000000pubb"; // display "b public"
const PUB_A: &str = "mmrsteamlistchan000000puba"; // display "a public"
/// Public in `Channels`, **no** `PublicChannels` row — the browse list must not see it.
const PUB_NO_SHADOW: &str = "mmrsteamlistchan00000noshd";
/// The shadow says team + living + display "0 drifted"; `Channels` says the other team,
/// archived, display "zzz channels-says". Every predicate in the query disagrees with the row.
const PUB_DRIFTED: &str = "mmrsteamlistchan00000drift";
/// Archived public: keeps its shadow row, with `DeleteAt` copied across.
const PUB_ARCHIVED: &str = "mmrsteamlistchan00000parch";

const PRIV_LIVING: &str = "mmrsteamlistchan00000privl"; // display "m private"
const PRIV_ARCHIVED_MEMBER: &str = "mmrsteamlistchan00000priva"; // archived, USER is a member
const PRIV_ARCHIVED_STRANGER: &str = "mmrsteamlistchan00000privs"; // archived, USER is not
const BOARD_LIVING: &str = "mmrsteamlistchan00000board"; // BO, living
const BOARD_ARCHIVED: &str = "mmrsteamlistchan0000barchd"; // BO, archived
const DM_ARCHIVED: &str = "mmrsteamlistchan000000dmar"; // D, archived, teamless

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsteamlist%'",
        "DELETE FROM publicchannels WHERE id LIKE 'mmrsteamlist%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsteamlist%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsteamlist%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

struct Seed {
    id: &'static str,
    team: &'static str,
    kind: &'static str,
    display_name: &'static str,
    delete_at: i64,
    member: bool,
    /// `(teamid, displayname, deleteat)` for the `PublicChannels` shadow row, when there is one.
    shadow: Option<(&'static str, &'static str, i64)>,
}

async fn seed(pool: &PgPool) {
    for (team, name) in [(TEAM, "mmrs-teamlist-a"), (OTHER_TEAM, "mmrs-teamlist-b")] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs teamlist', $2, 'O', false)",
        )
        .bind(team)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    for row in [
        Seed {
            id: PUB_B,
            team: TEAM,
            kind: "O",
            display_name: "b public",
            delete_at: 0,
            member: true,
            shadow: Some((TEAM, "b public", 0)),
        },
        Seed {
            id: PUB_A,
            team: TEAM,
            kind: "O",
            display_name: "a public",
            delete_at: 0,
            member: false,
            shadow: Some((TEAM, "a public", 0)),
        },
        Seed {
            id: PUB_NO_SHADOW,
            team: TEAM,
            kind: "O",
            display_name: "a0 no shadow",
            delete_at: 0,
            member: true,
            shadow: None,
        },
        Seed {
            id: PUB_DRIFTED,
            team: OTHER_TEAM,
            kind: "O",
            display_name: "zzz channels-says",
            delete_at: 777,
            member: false,
            shadow: Some((TEAM, "0 drifted", 0)),
        },
        Seed {
            id: PUB_ARCHIVED,
            team: TEAM,
            kind: "O",
            display_name: "c archived public",
            delete_at: 555,
            member: true,
            shadow: Some((TEAM, "c archived public", 555)),
        },
        Seed {
            id: PRIV_LIVING,
            team: TEAM,
            kind: "P",
            display_name: "m private",
            delete_at: 0,
            member: true,
            shadow: None,
        },
        Seed {
            id: PRIV_ARCHIVED_MEMBER,
            team: TEAM,
            kind: "P",
            display_name: "n archived private mine",
            delete_at: 556,
            member: true,
            shadow: None,
        },
        Seed {
            id: PRIV_ARCHIVED_STRANGER,
            team: TEAM,
            kind: "P",
            display_name: "o archived private theirs",
            delete_at: 557,
            member: false,
            shadow: None,
        },
        Seed {
            id: BOARD_LIVING,
            team: TEAM,
            kind: "BO",
            display_name: "l board",
            delete_at: 0,
            member: true,
            shadow: None,
        },
        Seed {
            id: BOARD_ARCHIVED,
            team: TEAM,
            kind: "BO",
            display_name: "p archived board",
            delete_at: 558,
            member: true,
            shadow: None,
        },
        Seed {
            id: DM_ARCHIVED,
            team: "",
            kind: "D",
            display_name: "q archived dm",
            delete_at: 559,
            member: true,
            shadow: None,
        },
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, $5, $6, 0, 0)",
        )
        .bind(row.id)
        .bind(row.delete_at)
        .bind(row.team)
        .bind(row.kind)
        .bind(row.display_name)
        .bind(format!("name-{}", row.id))
        .execute(pool)
        .await
        .expect("inserts the channel");

        if let Some((team, display_name, delete_at)) = row.shadow {
            sqlx::query(
                "INSERT INTO publicchannels (id, deleteat, teamid, displayname, name, header, purpose)
                 VALUES ($1, $2, $3, $4, $5, '', '')",
            )
            .bind(row.id)
            .bind(delete_at)
            .bind(team)
            .bind(display_name)
            .bind(format!("name-{}", row.id))
            .execute(pool)
            .await
            .expect("inserts the shadow row");
        }

        if row.member {
            sqlx::query(
                "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser)
                 VALUES ($1, $2, '', '{}'::jsonb, true)",
            )
            .bind(row.id)
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

/// The browse list believes `PublicChannels`, not `Channels`, about **all three** of the team,
/// the archived flag and the sort order — and a channel with no shadow row does not exist to it.
///
/// `PUB_DRIFTED` carries the whole argument on its own: `Channels` puts it in the other team,
/// archived, sorting last; the shadow puts it in this team, living, sorting first. It comes back,
/// and it comes back **first**. Every `pc.` → `channels.` mutation dies on this one row, and
/// none of them can be seen through the REST API, where the two tables always agree.
#[tokio::test]
async fn the_public_list_reads_the_shadow_table_not_the_channel_row() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let list = get_public_channels_for_team(&pool, TEAM, 0, 100)
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(&list),
        vec![PUB_DRIFTED, PUB_A, PUB_B],
        "the shadow's team, DeleteAt and DisplayName decide membership and order"
    );
    let drifted = &list.0[0];
    assert_eq!(
        drifted.display_name, "zzz channels-says",
        "the *row* still comes from Channels — only the predicates and the ORDER BY read pc"
    );
    assert_eq!(drifted.delete_at, 777);
    assert_eq!(drifted.team_id, OTHER_TEAM);
}

/// Offset paging, and the two ends of it: `LIMIT 0` is an empty page rather than "no limit", and
/// an offset past the end is an empty list rather than `NotFound`.
#[tokio::test]
async fn the_public_list_pages_by_offset_and_runs_out_empty() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let mut walked: Vec<String> = Vec::new();
    // Bounded: an off-by-one in the offset arithmetic must fail, not spin.
    for page in 0..6_i64 {
        let list = get_public_channels_for_team(&pool, TEAM, page * 2, 2)
            .await
            .expect("queries");
        if list.0.is_empty() {
            break;
        }
        walked.extend(list.0.iter().map(|c| c.id.clone()));
    }
    let zero_limit = get_public_channels_for_team(&pool, TEAM, 0, 0)
        .await
        .expect("queries");
    let past_the_end = get_public_channels_for_team(&pool, TEAM, 50, 60)
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(walked, vec![PUB_DRIFTED, PUB_A, PUB_B], "no row seen twice");
    assert!(zero_limit.0.is_empty(), "LIMIT 0 is a real limit");
    assert!(
        past_the_end.0.is_empty(),
        "an offset past the end is [] — not the NotFound GetChannels answers with"
    );
}

/// `Type = 'P'` exactly: not `<> 'O'`, and not `messageChannelTypes`. A board in the team is not
/// a private channel, and neither is an archived private one.
#[tokio::test]
async fn the_private_list_is_exactly_living_private_channels() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let list = get_private_channels_for_team(&pool, TEAM, 0, 100)
        .await
        .expect("queries");
    let other = get_private_channels_for_team(&pool, OTHER_TEAM, 0, 100)
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(&list),
        vec![PRIV_LIVING],
        "the board, the archived private channels and every public one stay out"
    );
    assert!(
        other.0.is_empty(),
        "and the predicate is on Channels.TeamId"
    );
}

/// `skipTeamMembershipCheck` widens by **type**, not merely by membership: with it, archived DMs
/// and every archived private channel are listed; without it, `O` plus the private ones the user
/// still holds a membership row for, and no `D` or `G` at all. Boards are excluded either way.
#[tokio::test]
async fn the_deleted_list_narrows_by_type_then_by_membership() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let wide = get_deleted(&pool, TEAM, 0, 100, USER, true)
        .await
        .expect("queries");
    let narrow = get_deleted(&pool, TEAM, 0, 100, USER, false)
        .await
        .expect("queries");
    let stranger = get_deleted(&pool, TEAM, 0, 100, "mmrsteamlistuser0000000002", false)
        .await
        .expect("queries");
    let empty_page = get_deleted(&pool, TEAM, 99, 100, USER, true)
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(&wide),
        vec![
            PUB_ARCHIVED,
            PRIV_ARCHIVED_MEMBER,
            PRIV_ARCHIVED_STRANGER,
            DM_ARCHIVED
        ],
        "manage_system sees every archived message channel, the teamless DM included; \
         the archived board is not one"
    );
    assert_eq!(
        ids(&narrow),
        vec![PUB_ARCHIVED, PRIV_ARCHIVED_MEMBER],
        "without the skip: every archived public channel, plus one's own archived private ones"
    );
    assert_eq!(
        ids(&stranger),
        vec![PUB_ARCHIVED],
        "a user with no membership row keeps the public one — membership is not the gate for O"
    );
    assert!(empty_page.0.is_empty(), "and a page past the end is []");
}
