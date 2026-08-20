//! `SqlChannelStore::get_by_names` against a real Postgres, on fixtures the REST API cannot
//! build or that the route in front of it never exercises.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_get_by_names
//! ```
//!
//! # What the cross-server suite covers, and what only this file can
//!
//! `crates/mm-api/tests/parity_channel_get.rs` reaches this query through `FillInChannelProps`
//! and covers the archived and missing-name cases against the running Go server. Two predicates
//! are not reachable that way:
//!
//! - **`Type IN (O, P, D, G)`** — a board (`BO`) cannot be created through the REST API on Team
//!   Edition, so the type filter never fires over HTTP. Same weakening as [D-151]: this is
//!   transcribed from `messageChannelTypes` (channel_store.go:39), not measured against Go.
//! - **The empty-team wildcard.** Go *omits* the `TeamId` predicate when `teamId == ""`
//!   (channel_store.go:1656) — the DM/GM case — and the port folds that into `$2 = '' OR …`.
//!   A header mention in a DM would exercise it over HTTP, but proving "matched a channel on a
//!   *different* team" needs two teams with a same-named channel, which is cheaper to pin here.
//!
//! # Every row here is `mmrs`-prefixed and removed before and after

use mm_store::channel_store::get_by_names;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The four tests share one set of fixture rows and each purges before seeding, so two running
/// interleaved delete each other's fixtures mid-assertion. Serialised here rather than by asking
/// the operator for `--test-threads=1`, which is exactly the kind of instruction that gets lost.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM_A: &str = "mmrsbynamesteam0000000aaaa";
const TEAM_B: &str = "mmrsbynamesteam0000000bbbb";
const OPEN_A: &str = "mmrsbynameschan0000000open";
const OPEN_B: &str = "mmrsbynameschan000000open2";
const PRIVATE_A: &str = "mmrsbynameschan0000000priv";
const BOARD_A: &str = "mmrsbynameschan0000000bord";
const ARCHIVED_A: &str = "mmrsbynameschan0000000arch";

/// The one name that exists on both teams; the team filter is what separates its two rows.
const SHARED_NAME: &str = "mmrs-bynames-shared";

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
        "DELETE FROM channels WHERE id LIKE 'mmrsbynames%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsbynames%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Two teams. On team A: an open channel named `shared`, a private one, a board, and an archived
/// open one — each under its own name except where the test needs a collision. On team B: an
/// open channel with team A's shared name.
async fn seed(pool: &PgPool) {
    for (team, name) in [(TEAM_A, "mmrs-bynames-a"), (TEAM_B, "mmrs-bynames-b")] {
        sqlx::query(
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type, allowopeninvite)
             VALUES ($1, 0, 0, 0, 'mmrs bynames', $2, 'O', false)",
        )
        .bind(team)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the team");
    }

    for (id, team, channel_type, name, delete_at) in [
        (OPEN_A, TEAM_A, "O", SHARED_NAME, 0_i64),
        (OPEN_B, TEAM_B, "O", SHARED_NAME, 0),
        (PRIVATE_A, TEAM_A, "P", "mmrs-bynames-private", 0),
        (BOARD_A, TEAM_A, "BO", "mmrs-bynames-board", 0),
        (
            ARCHIVED_A,
            TEAM_A,
            "O",
            "mmrs-bynames-archived",
            1_700_000_000_000,
        ),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot)
             VALUES ($1, 0, 0, $2, $3, $4::channel_type, 'mmrs bynames', $5, 0, 0)",
        )
        .bind(id)
        .bind(delete_at)
        .bind(team)
        .bind(channel_type)
        .bind(name)
        .execute(pool)
        .await
        .expect("inserts the channel");
    }
}

fn ids(mut channels: Vec<mm_model::channel::Channel>) -> Vec<String> {
    channels.sort_by(|a, b| a.id.cmp(&b.id));
    channels.into_iter().map(|c| c.id).collect()
}

/// A non-empty team id scopes the lookup: the same name on another team is not a match.
#[tokio::test]
async fn a_team_id_excludes_the_other_teams_channel_of_the_same_name() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let found = get_by_names(&pool, TEAM_A, &[SHARED_NAME.to_owned()])
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(ids(found), vec![OPEN_A.to_owned()]);
}

/// Go **omits** the predicate for an empty team id rather than comparing against `''` — the
/// DM/GM case, whose `TeamId` is `""` — so both teams' rows come back.
#[tokio::test]
async fn an_empty_team_id_searches_every_team() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let found = get_by_names(&pool, "", &[SHARED_NAME.to_owned()])
        .await
        .expect("queries");

    purge(&pool).await;
    assert_eq!(ids(found), vec![OPEN_A.to_owned(), OPEN_B.to_owned()]);
}

/// The exported `GetByNames` is the non-archived variant, and the type filter is
/// `messageChannelTypes`: a private channel **is** returned (the app layer drops it from the
/// prop, not the store), a board is not, an archived channel is not.
#[tokio::test]
async fn the_type_and_deleteat_filters_are_the_stores_not_the_apps() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let found = get_by_names(
        &pool,
        TEAM_A,
        &[
            "mmrs-bynames-private".to_owned(),
            "mmrs-bynames-board".to_owned(),
            "mmrs-bynames-archived".to_owned(),
        ],
    )
    .await
    .expect("queries");

    purge(&pool).await;
    assert_eq!(
        ids(found),
        vec![PRIVATE_A.to_owned()],
        "P passes the type filter; BO fails it; the archived O fails DeleteAt = 0"
    );
}

/// `len(names) > 0` short-circuits in Go before any SQL; an empty list is an empty answer even
/// with a working pool, and a name that matches nothing is simply absent rather than an error.
#[tokio::test]
async fn empty_and_missing_names_are_empty_answers_not_errors() {
    if !db_enabled() {
        eprintln!("skipping: set MM_STORE_DB=1 with DATABASE_URL pointing at the stack");
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let none = get_by_names(&pool, TEAM_A, &[]).await.expect("no query");
    let missing = get_by_names(&pool, TEAM_A, &["mmrs-bynames-no-such".to_owned()])
        .await
        .expect("queries");
    let mixed = get_by_names(
        &pool,
        TEAM_A,
        &[SHARED_NAME.to_owned(), "mmrs-bynames-no-such".to_owned()],
    )
    .await
    .expect("queries");

    purge(&pool).await;
    assert!(none.is_empty());
    assert!(missing.is_empty());
    assert_eq!(
        ids(mixed),
        vec![OPEN_A.to_owned()],
        "the hit survives the miss beside it"
    );
}
