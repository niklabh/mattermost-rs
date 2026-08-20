//! `SqlTeamStore.GetByName`, `GetMember` and `GetMembers` against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_members
//! ```
//!
//! The branches the SQL carries and REST cannot drive one at a time: the membership `DeleteAt`
//! filter that `GetMembers` applies and `GetMember` does not, the `Users.DeleteAt` filter behind
//! `exclude_deleted_users`, the three-way sort (`UserId`, `Username`, nothing), the unguarded
//! `LIMIT 0`, and `GetByName`'s exact-match-no-folding lookup that still serves an archived team.

use mm_store::team_store::{TeamMembersGetOptions, get_by_name, get_member, get_members};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Same file-local serialisation as every other DB suite.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM: &str = "mmrstmteam000000000000main";
const ARCHIVED_TEAM: &str = "mmrstmteam000000000000arch";
const OTHER_TEAM: &str = "mmrstmteam00000000000other";
const TEAM_NAME: &str = "mmrstm-members-main";
const ARCHIVED_TEAM_NAME: &str = "mmrstm-members-archived";

// User ids chosen so that **id order and username order disagree**: a sort mutation that
// picks the wrong column cannot coincide with the right one.
const USER_A: &str = "mmrstmuser0000000000000aaa"; // username zulu
const USER_B: &str = "mmrstmuser0000000000000bbb"; // username mike
const USER_C: &str = "mmrstmuser0000000000000ccc"; // username alpha, deactivated
const USER_DEPARTED: &str = "mmrstmuser0000000000000ddd"; // membership soft-deleted

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
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrstm%' OR userid LIKE 'mmrstm%'",
        "DELETE FROM teams WHERE id LIKE 'mmrstm%'",
        "DELETE FROM users WHERE id LIKE 'mmrstm%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

async fn insert_team(pool: &PgPool, id: &str, name: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite, email, inviteid)
         VALUES ($1, 1700000000001, 1700000000002, $3, 'mmrs team members', $2, 'O', false, 'owner@mmrs.invalid', $1)",
    )
    .bind(id)
    .bind(name)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the team");
}

async fn insert_user(pool: &PgPool, id: &str, username: &str, delete_at: i64) {
    sqlx::query(
        "INSERT INTO users (id, createat, updateat, deleteat, username, email, roles, lastlogin)
         VALUES ($1, 0, 0, $3, $2, $1 || '@mmrs.invalid', 'system_user', 0)",
    )
    .bind(id)
    .bind(username)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the user");
}

async fn insert_member(pool: &PgPool, team_id: &str, user_id: &str, delete_at: i64, admin: bool) {
    sqlx::query(
        "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin, schemeguest, createat)
         VALUES ($1, $2, '', $3, true, $4, false, 1700000000003)",
    )
    .bind(team_id)
    .bind(user_id)
    .bind(delete_at)
    .bind(admin)
    .execute(pool)
    .await
    .expect("inserts the membership");
}

async fn seed(pool: &PgPool) {
    insert_team(pool, TEAM, TEAM_NAME, 0).await;
    insert_team(pool, ARCHIVED_TEAM, ARCHIVED_TEAM_NAME, 1_700_000_000_000).await;
    insert_team(pool, OTHER_TEAM, "mmrstm-members-other", 0).await;

    insert_user(pool, USER_A, "mmrstm-zulu", 0).await;
    insert_user(pool, USER_B, "mmrstm-mike", 0).await;
    insert_user(pool, USER_C, "mmrstm-alpha", 1_700_000_000_000).await;
    insert_user(pool, USER_DEPARTED, "mmrstm-departed", 0).await;

    insert_member(pool, TEAM, USER_A, 0, true).await;
    insert_member(pool, TEAM, USER_B, 0, false).await;
    insert_member(pool, TEAM, USER_C, 0, false).await;
    insert_member(pool, TEAM, USER_DEPARTED, 1_700_000_000_000, false).await;
    insert_member(pool, OTHER_TEAM, USER_A, 0, false).await;
}

fn ids(members: &[mm_model::team_member::TeamMember]) -> Vec<&str> {
    members.iter().map(|m| m.user_id.as_str()).collect()
}

/// Every branch of the member reads in one seeded database; one lock, one seed, in sequence.
#[tokio::test]
async fn the_member_reads_match_gos_sql() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL to run");
        return;
    }
    let _serialised = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    // Default sort: by UserId, departed member excluded, deactivated user included, other team's
    // rows absent.
    let default = get_members(&pool, TEAM, 0, 60, &TeamMembersGetOptions::default())
        .await
        .expect("lists");
    assert_eq!(
        ids(&default),
        vec![USER_A, USER_B, USER_C],
        "UserId order; the departed row is filtered, the deactivated user is not"
    );
    let admin = default
        .iter()
        .find(|m| m.user_id == USER_A)
        .expect("present");
    assert_eq!(admin.roles, "team_user team_admin");
    assert!(admin.scheme_admin && admin.scheme_user);
    assert_eq!(admin.create_at, 1_700_000_000_003);

    // Username sort: alpha (C), mike (B), zulu (A) — the reverse of id order by construction.
    let by_username = get_members(
        &pool,
        TEAM,
        0,
        60,
        &TeamMembersGetOptions {
            sort: mm_model::team_member::USERNAME.to_owned(),
            exclude_deleted_users: false,
        },
    )
    .await
    .expect("lists");
    assert_eq!(ids(&by_username), vec![USER_C, USER_B, USER_A]);

    // exclude_deleted_users drops the deactivated user's live membership.
    let active_only = get_members(
        &pool,
        TEAM,
        0,
        60,
        &TeamMembersGetOptions {
            sort: String::new(),
            exclude_deleted_users: true,
        },
    )
    .await
    .expect("lists");
    assert_eq!(ids(&active_only), vec![USER_A, USER_B]);

    // Both together.
    let active_by_username = get_members(
        &pool,
        TEAM,
        0,
        60,
        &TeamMembersGetOptions {
            sort: mm_model::team_member::USERNAME.to_owned(),
            exclude_deleted_users: true,
        },
    )
    .await
    .expect("lists");
    assert_eq!(ids(&active_by_username), vec![USER_B, USER_A]);

    // A sort value that is neither "" nor "Username" orders by nothing — the set is unchanged,
    // the order is whatever the heap gives, so only membership is asserted.
    let unsorted_members = get_members(
        &pool,
        TEAM,
        0,
        60,
        &TeamMembersGetOptions {
            sort: "username".to_owned(),
            exclude_deleted_users: false,
        },
    )
    .await
    .expect("lists");
    let mut unsorted = ids(&unsorted_members);
    unsorted.sort_unstable();
    assert_eq!(unsorted, vec![USER_A, USER_B, USER_C]);

    // Pagination: LIMIT is unguarded, so 0 means zero rows (Go emits `LIMIT 0`); offset pages.
    let none = get_members(&pool, TEAM, 0, 0, &TeamMembersGetOptions::default())
        .await
        .expect("lists");
    assert!(
        none.is_empty(),
        "LIMIT 0 is an empty page here, unlike the channel store"
    );
    let page_two = get_members(&pool, TEAM, 2, 2, &TeamMembersGetOptions::default())
        .await
        .expect("lists");
    assert_eq!(
        ids(&page_two),
        vec![USER_C],
        "offset 2, limit 2 of three rows"
    );
    let page_past_end = get_members(&pool, TEAM, 10, 2, &TeamMembersGetOptions::default())
        .await
        .expect("lists");
    assert!(page_past_end.is_empty());

    // A team with no rows — or no team — is an empty list, not an error.
    let missing = get_members(
        &pool,
        "mmrstmteam0000000000nosuch",
        0,
        60,
        &TeamMembersGetOptions::default(),
    )
    .await
    .expect("lists");
    assert!(missing.is_empty());

    // GetMember has no DeleteAt filter: the departed row answers, with its delete_at.
    let departed = get_member(&pool, TEAM, USER_DEPARTED).await.expect("found");
    assert_eq!(departed.delete_at, 1_700_000_000_000);
    assert_eq!(departed.roles, "team_user");
    let miss = get_member(&pool, OTHER_TEAM, USER_B)
        .await
        .expect_err("not a member of the other team");
    assert!(miss.is_not_found(), "{miss:?}");

    // GetByName: exact match, archived team still served, no case folding.
    let team = get_by_name(&pool, TEAM_NAME).await.expect("found");
    assert_eq!(team.id, TEAM);
    assert_eq!(team.email, "owner@mmrs.invalid");
    assert_eq!(team.create_at, 1_700_000_000_001);
    let archived = get_by_name(&pool, ARCHIVED_TEAM_NAME)
        .await
        .expect("an archived team answers by name");
    assert_eq!(archived.id, ARCHIVED_TEAM);
    assert_eq!(archived.delete_at, 1_700_000_000_000);
    let folded = get_by_name(&pool, "MMRSTM-MEMBERS-MAIN")
        .await
        .expect_err("Name = $1 is exact; nothing lowercases the parameter");
    assert!(folded.is_not_found(), "{folded:?}");
    let nosuch = get_by_name(&pool, "mmrstm-no-such-team")
        .await
        .expect_err("missing");
    assert!(nosuch.is_not_found(), "{nosuch:?}");

    purge(&pool).await;
}
