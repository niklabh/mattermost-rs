//! `SqlConfigStore::load_active` against the row a real Go server wrote.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_config_active
//! ```
//!
//! # The active row is read, never written
//!
//! Every other `db_*` test writes `mmrs`-prefixed fixtures and purges them. Four of the five here
//! write nothing at all: they read the configuration the Go server booted itself on, which is a
//! row this repo must not author — the moment we write it we are back to asserting against our
//! own beliefs rather than against Go's.
//!
//! The exception is [`a_superseded_revision_is_not_returned`], which seeds an **inactive** row and
//! removes it again. That is the one shape the active row's own existence cannot test: with a
//! single row in the table, `WHERE active` and `WHERE true` are indistinguishable. It still never
//! touches the active row.
//!
//! Between them they cover the whole contract: the row exists and is unique, a superseded one is
//! ignored, it parses, its modelled values match the committed fixture, and the two claims
//! `mm_app::config` makes about what Go does *not* persist still hold.

use mm_store::{ConfigStore, SqlConfigStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

/// The projection `scripts/dump-config-fixture.sh` writes, and what `mm_app::config` parses.
const FIXTURE: &str = include_str!("../../../fixtures/config_active.json");

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        // Capped well under sqlx's 30-second default: a suite that waits half a minute to
        // discover an unreachable database is the bug CLAUDE.md's "a test that waits is a bug"
        // is about.
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

/// The row is there and `load_active` finds it.
///
/// A `None` here does not mean the store is broken — it almost certainly means `MM_CONFIG` is not
/// pointed at this database and the Go server is still on the `config.json` file store, which is
/// the state this whole change exists to leave behind. The message says so.
#[tokio::test]
async fn the_active_configuration_is_readable() {
    if !db_enabled() {
        return;
    }
    let store = SqlConfigStore::new(pool().await);

    let document = store.load_active().await.expect("the query runs");

    let document = document.expect(
        "no active row in Configurations — is MM_CONFIG pointed at this database? \
         See the MM_CONFIG comment in docker-compose.yml",
    );
    assert!(
        document.len() > 10_000,
        "a whole model.Config is tens of kilobytes; got {} bytes",
        document.len()
    );
    let parsed: serde_json::Value = serde_json::from_str(&document).expect("the row is JSON");
    assert!(
        parsed.as_object().is_some_and(|o| o.len() > 40),
        "model.Config has 47 top-level sections"
    );
}

/// Exactly one row is active, which is what `WHERE active` depends on.
///
/// Go deactivates by setting `Active = NULL` rather than `false` (config/database.go:199) and
/// leans on the UNIQUE constraint to keep a single `true`. If that ever became `false`,
/// `load_active` would start returning superseded revisions and every setting would silently go
/// stale — `fetch_optional` would not even error, because there would still be exactly one row
/// matching for a while.
#[tokio::test]
async fn exactly_one_row_is_active() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;

    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM configurations WHERE active")
        .fetch_one(&pool)
        .await
        .expect("counts");
    assert_eq!(active, 1, "exactly one configuration may be active");

    let falses: i64 =
        sqlx::query_scalar("SELECT count(*) FROM configurations WHERE active = false")
            .fetch_one(&pool)
            .await
            .expect("counts");
    assert_eq!(
        falses, 0,
        "Go deactivates with NULL, not false — a false here means something other than the Go \
         server is writing this table, and `WHERE active` is no longer the right predicate"
    );
}

/// The committed fixture still matches the live row, key for key.
///
/// This is what stops `fixtures/config_active.json` rotting into a record of what Go used to do.
/// A failure means either the Go server changed a default — which is a real finding and belongs in
/// `MIGRATION.md` — or someone edited the configuration of the development stack, in which case
/// re-run `scripts/dump-config-fixture.sh`.
#[tokio::test]
async fn the_committed_fixture_still_matches_the_live_row() {
    if !db_enabled() {
        return;
    }
    let store = SqlConfigStore::new(pool().await);
    let document = store
        .load_active()
        .await
        .expect("the query runs")
        .expect("an active row");

    let live: serde_json::Value = serde_json::from_str(&document).expect("the row is JSON");
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");

    for (section, keys) in fixture.as_object().expect("the fixture is an object") {
        for (key, expected) in keys.as_object().expect("each section is an object") {
            let actual = live
                .get(section)
                .and_then(|s| s.get(key))
                .unwrap_or_else(|| panic!("{section}.{key} is absent from the live row"));
            assert_eq!(
                actual, expected,
                "{section}.{key} drifted — re-run scripts/dump-config-fixture.sh, and check \
                 whether Go changed a default before you do"
            );
        }
    }
}

/// A superseded revision must not be returned.
///
/// **Without this the `WHERE active` predicate is untestable**, because a stack that has booted
/// once has exactly one row — so `WHERE active`, `WHERE active IS NOT FALSE` and `WHERE true` all
/// return the same document and a mutation of the predicate survives. Go's deactivation writes
/// `Active = NULL` (config/database.go:199), so this seeds precisely that shape: a second row that
/// a correct predicate ignores and a careless one may pick.
///
/// This is the one place in this file that writes to `Configurations`, and it writes only the row
/// Go itself would leave behind after a config change. The active row is never touched, and the
/// seeded row is removed either way.
///
/// # What this test still cannot catch, measured
///
/// Two mutations of the predicate — `WHERE active IS NOT FALSE`, and dropping the `WHERE` clause
/// altogether — **survive** this test, and the cause is physical row order rather than a weak
/// assertion. The active row was written at first boot and sits at `ctid (0,1)`; a row seeded here
/// lands after it, at `(0,6)` when this was measured. A widened predicate matches both rows, the
/// sequential scan reaches the active one first, and `fetch_optional` takes it and ignores the
/// rest — so the query returns the right answer for the wrong reason.
///
/// This is not unreachable in production, which is why it is recorded rather than dismissed: a
/// long-lived server accumulates superseded revisions, and any reordering (a `VACUUM FULL`, a
/// `CLUSTER`, a restore from dump) can put one of them first. Closing it needs the seeded row to
/// precede the active one physically, which this suite cannot arrange without rewriting a row that
/// belongs to the Go server. See the tally in `MIGRATION.md`.
#[tokio::test]
async fn a_superseded_revision_is_not_returned() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let store = SqlConfigStore::new(pool.clone());

    // 26-character base32-ish id in this repo's `mmrs` fixture namespace, so `purge_api_fixtures`
    // and a human reading the table both know where it came from.
    const SUPERSEDED_ID: &str = "mmrsconfigsuperseded000001";
    const MARKER: &str = r#"{"ServiceSettings":{"SiteURL":"http://superseded.invalid"}}"#;

    sqlx::query("DELETE FROM configurations WHERE id = $1")
        .bind(SUPERSEDED_ID)
        .execute(&pool)
        .await
        .expect("cleans up any leftover from a failed run");

    sqlx::query(
        "INSERT INTO configurations (id, value, createat, active, sha) \
         VALUES ($1, $2, $3, NULL, $4)",
    )
    .bind(SUPERSEDED_ID)
    .bind(MARKER)
    .bind(1_i64)
    .bind("0".repeat(64))
    .execute(&pool)
    .await
    .expect("seeds a superseded revision");

    let loaded = store.load_active().await;

    sqlx::query("DELETE FROM configurations WHERE id = $1")
        .bind(SUPERSEDED_ID)
        .execute(&pool)
        .await
        .expect("removes the seeded row");

    let document = loaded
        .expect("the query runs")
        .expect("the active row is still found alongside an inactive one");
    assert!(
        !document.contains("superseded.invalid"),
        "load_active returned a superseded revision — `WHERE active` has been widened into \
         something that matches Go's deactivated rows (they carry Active = NULL, not false)"
    );
    assert!(
        document.len() > 10_000,
        "and it is still the real document, not the marker"
    );
}

/// `FeatureFlags` is not persisted, so it can never be read from here.
///
/// `Store.Load` clears the section on all three configs before comparing or persisting when
/// `readOnlyFF` is set, which is the default (config/store.go:306-310). `mm_app::config` documents
/// this and deliberately sources `feature_flag_burn_on_read` from the environment instead; if the
/// section ever starts appearing, that decision needs revisiting and this test is the alarm.
#[tokio::test]
async fn feature_flags_are_absent_from_the_persisted_document() {
    if !db_enabled() {
        return;
    }
    let store = SqlConfigStore::new(pool().await);
    let document = store
        .load_active()
        .await
        .expect("the query runs")
        .expect("an active row");
    let parsed: serde_json::Value = serde_json::from_str(&document).expect("the row is JSON");

    assert!(
        parsed.get("FeatureFlags").is_none(),
        "the persisted config now carries FeatureFlags — mm_app::config sources feature flags \
         from the environment on the strength of its absence, and that is no longer safe"
    );
}
