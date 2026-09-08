//! `team_store::get_by_invite_id`'s **empty-invite guard**, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_invite_id
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! Go guards the lookup with `if inviteId == "" || team.InviteId != inviteId` (team_store.go:403),
//! and the empty half is the one that matters: **`Teams.InviteId` is not unique** — Go ships a
//! `GetByEmptyInviteID` precisely because rows with an empty one occur — so without the guard an
//! empty parameter returns an arbitrary team to an **unauthenticated** caller of
//! `GET /api/v4/teams/invite/{invite_id}`.
//!
//! No HTTP request can reach it: an empty path segment is not a route on either server, so the
//! router 404s first. A mutation deleting the guard therefore survived the parity suite. Here the
//! store is called directly with `""`.
//!
//! Every test is named `team_invite_id_*` so `MUTATE_FILTER` can select them by name.

use mm_store::team_store;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM_WITH_INVITE: &str = "mmrsinvteamwithxxxxxxxxxxx";
const TEAM_WITHOUT_INVITE: &str = "mmrsinvteamwithoutxxxxxxxx";
const INVITE: &str = "mmrsinviteidxxxxxxxxxxxxxx";

fn enabled() -> bool {
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
    sqlx::query("DELETE FROM teams WHERE id LIKE 'mmrsinv%'")
        .execute(pool)
        .await
        .expect("purges leftover fixtures");
}

/// Two teams: one with an invite id, one whose invite id is the empty string.
///
/// **`LastTeamIconUpdate` is written**, not left NULL: Go scans it into an `int64`, and a NULL
/// there makes Go's own `GET /api/v4/teams` a 500 for as long as the row exists. That has happened
/// once already, from `db_authorization_by_post`.
async fn seed(pool: &PgPool) {
    purge(pool).await;

    for (id, invite) in [(TEAM_WITH_INVITE, INVITE), (TEAM_WITHOUT_INVITE, "")] {
        sqlx::query(
            r#"
            INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                               email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                               schemeid, lastteamiconupdate, groupconstrained, cloudlimitsarchived)
            VALUES ($1, 1755000000000, 1755000000000, 0, 'mmrs invite fixture', $2, '', '',
                    'O', '', '', $3, true, NULL, 0, false, false)
            "#,
        )
        .bind(id)
        .bind(format!("mmrs-inv-{}", &id[7..17]))
        .bind(invite)
        .execute(pool)
        .await
        .expect("inserts a test team");
    }
}

/// **The guard.** An empty invite id must find nothing, even though a row with an empty
/// `InviteId` exists and the query alone would happily return it.
#[tokio::test]
async fn team_invite_id_empty_matches_nothing() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    // The row is really there, and the query alone really would return it — otherwise this test
    // would pass for the wrong reason.
    let planted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM teams WHERE inviteid = '' AND id LIKE 'mmrsinv%'")
            .fetch_one(&pool)
            .await
            .expect("counts");
    assert_eq!(
        planted, 1,
        "the fixture plants a team with an empty invite id"
    );

    let err = team_store::get_by_invite_id(&pool, "")
        .await
        .expect_err("an empty invite id must not match a team");
    assert!(err.is_not_found(), "and the answer is not-found: {err}");

    purge(&pool).await;
}

/// The positive control: a real invite id finds its team, and a wrong one finds nothing.
#[tokio::test]
async fn team_invite_id_finds_its_team_and_only_its_team() {
    if !enabled() {
        return;
    }
    let _db = DB.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    let team = team_store::get_by_invite_id(&pool, INVITE)
        .await
        .expect("the team is found");
    assert_eq!(team.id, TEAM_WITH_INVITE);
    assert_eq!(team.invite_id, INVITE);

    let err = team_store::get_by_invite_id(&pool, "mmrsinvitenosuchxxxxxxxxxx")
        .await
        .expect_err("no such invite");
    assert!(err.is_not_found());

    purge(&pool).await;
}
