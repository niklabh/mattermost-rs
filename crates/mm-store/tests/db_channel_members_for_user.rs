//! `SqlChannelStore::get_members_for_user` against a real Postgres, on the branches the route in
//! front of it never reaches.
//!
//! ```sh
//! scripts/parity.sh -p mm-store --test db_channel_members_for_user
//! ```
//!
//! `crates/mm-api/tests/parity_channel_members_for_team_for_user.rs` covers the DM-in-every-team
//! rule, the archived membership, the sanitizer and the empty `[]` against the running Go server.
//! Transcribed from `SqlChannelStore.GetMembersForUser` (channel_store.go:3261) rather than
//! measured, because nothing over REST creates them:
//!
//! - **`Channels.Type NOT IN ('S')`** — a Space's backing channel is excluded; a board (`BO`)
//!   is **not**, unlike the sibling channel list's allow-list.
//! - **The team predicate is `Teams.Id` through the LEFT join**, so a membership whose channel
//!   names a team row that does not exist arrives as NULL and is listed under *every* team.
//! - A membership whose channel row is gone is dropped by the INNER join.

use mm_store::channel_store::get_members_for_user;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const USER: &str = "mmrsmemforuser000000000001";
const TEAM_A: &str = "mmrsmemforteam00000000aaaa";
const TEAM_B: &str = "mmrsmemforteam00000000bbbb";
const TEAM_GONE: &str = "mmrsmemforteam00000000gone"; // never inserted

const A_OPEN: &str = "mmrsmemforchan000000aopen1"; // team A, O
const A_BOARD: &str = "mmrsmemforchan000000aboard"; // team A, BO — listed
const A_SPACE: &str = "mmrsmemforchan000000aspace"; // team A, S — excluded
const A_ARCHIVED: &str = "mmrsmemforchan000000aarchv"; // team A, O, deleteat > 0 — listed
const A_NOT_MEMBER: &str = "mmrsmemforchan000000anomem"; // team A, O, no membership row
const DM: &str = "mmrsmemforchan000000dm0001"; // team "", D — listed under every team
const GONE_TEAM: &str = "mmrsmemforchan000000gonet1"; // team that does not exist — every team
const B_OPEN: &str = "mmrsmemforchan000000bopen1"; // team B, O
const ORPHAN: &str = "mmrsmemforchan000000orphan"; // membership only, no channel row

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
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsmemfor%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsmemfor%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsmemfor%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn seed(pool: &PgPool) {
    for (team, name) in [(TEAM_A, "mmrs-memfor-a"), (TEAM_B, "mmrs-memfor-b")] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs memfor', $2, 'O', false)",
        )
        .bind(team)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    for (id, team, channel_type, delete_at, member) in [
        (A_OPEN, TEAM_A, "O", 0_i64, true),
        (A_BOARD, TEAM_A, "BO", 0, true),
        (A_SPACE, TEAM_A, "S", 0, true),
        (A_ARCHIVED, TEAM_A, "O", 1_700_000_000_000, true),
        (A_NOT_MEMBER, TEAM_A, "O", 0, false),
        (DM, "", "D", 0, true),
        (GONE_TEAM, TEAM_GONE, "O", 0, true),
        (B_OPEN, TEAM_B, "O", 0, true),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, $1, $1, 0, 0)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(team)
        .bind(channel_type)
        .execute(pool)
        .await
        .expect("inserts the channel");

        if member {
            insert_membership(pool, id).await;
        }
    }
    insert_membership(pool, ORPHAN).await;
}

async fn insert_membership(pool: &PgPool, channel_id: &str) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser)
         VALUES ($1, $2, '', '{}'::jsonb, true)",
    )
    .bind(channel_id)
    .bind(USER)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

fn channel_ids(members: &[mm_model::channel_member::ChannelMember]) -> Vec<&str> {
    let mut ids: Vec<&str> = members.iter().map(|m| m.channel_id.as_str()).collect();
    ids.sort_unstable();
    ids
}

/// Team A: the open channel, the board, the archived channel, the DM and the dangling-team
/// channel — and **not** the Space, the non-member channel, team B's channel or the orphan.
#[tokio::test]
async fn a_team_lists_every_membership_but_spaces_and_other_teams() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let members = get_members_for_user(&pool, TEAM_A, USER)
        .await
        .expect("queries");

    purge(&pool).await;
    let mut expected = vec![A_OPEN, A_BOARD, A_ARCHIVED, DM, GONE_TEAM];
    expected.sort_unstable();
    assert_eq!(channel_ids(&members), expected);
    assert!(members.iter().all(|m| m.user_id == USER));
}

/// Team B sees its own channel plus the two NULL-team rows — the DM and the dangling-team
/// channel are in **every** team's answer.
#[tokio::test]
async fn teamless_and_dangling_team_memberships_appear_under_every_team() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let members = get_members_for_user(&pool, TEAM_B, USER)
        .await
        .expect("queries");

    purge(&pool).await;
    let mut expected = vec![B_OPEN, DM, GONE_TEAM];
    expected.sort_unstable();
    assert_eq!(channel_ids(&members), expected);
}

/// A user with no rows at all is an empty list, not an error.
#[tokio::test]
async fn an_unknown_user_is_an_empty_list() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let pool = pool().await;
    let members = get_members_for_user(&pool, TEAM_A, "mmrsmemfornobody0000000001")
        .await
        .expect("queries");
    assert!(members.is_empty());
}
