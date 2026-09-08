//! `SqlJobStore::get_by_type_and_data`'s **JSONB predicate**, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_job_data_filter
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! The only two callers are `getJobsByType`'s `team_id` and `policy_id` branches, and both filter
//! **access-control** job types. Creating one of those needs a licence, so on Team Edition the
//! `Jobs` table holds no row with a `team_id` or `policy_id` in its `Data` — every filter matches
//! zero rows, and a mutation that breaks the predicate returns the same empty list as the correct
//! one. Two mutations survived the whole `api` suite that way: replacing the compared value, and
//! anything that would have changed *which* rows matched.
//!
//! The branch is not unreachable in principle — a licensed server writes these rows — so it is
//! tested where it *is* reachable: at the store, with rows planted directly.
//!
//! # Every test here is named `job_data_filter_*` on purpose
//!
//! `MUTATE_FILTER` filters test **names**, not test targets: a filter of `db_job_data_filter` —
//! this file's name — matches no test function, so cargo runs zero tests, exits 0, and every
//! mutation is reported SURVIVED. That happened on this plan's first run. The shared prefix gives
//! the harness a name it can actually select on.
//!
//! # The predicate compares JSONB to JSONB
//!
//! Go builds the right-hand side as `fmt.Sprintf("\"%s\"", value)` — a JSON **string literal**,
//! not the bare text (job_store.go:474). `Data->'team_id' = 'someteam'` is a syntax error as JSONB
//! and `Data->>'team_id' = 'someteam'` would be a different operator; only the quoted form is what
//! Go sends. A fixture where every row matches cannot tell those apart, so these rows differ in
//! exactly the compared key.

use mm_store::{JobStore, SqlJobStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TEAM_A: &str = "mmrsjobteamaaaaaaaaaaaaaaa";
const TEAM_B: &str = "mmrsjobteambbbbbbbbbbbbbbb";
/// **Deliberately not a real job type.** The two callers filter `access_control_sync` and
/// `access_control_team_sync`, and the parity suite asserts that both of those answer `[]` on this
/// deployment. A row of a real type left behind by a panicking test would break that suite from a
/// different binary; a synthetic type is invisible to every API route — `getJobsByType` 400s on it
/// and `getJobs` filters by `AllJobTypes` — so a leaked row can only ever affect this file. The
/// store's predicate does not care which string it is.
const TYPE: &str = "mmrs_data_filter_a";
const OTHER_TYPE: &str = "mmrs_data_filter_b";

fn db_enabled() -> bool {
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
    sqlx::query("DELETE FROM jobs WHERE id LIKE 'mmrsjob%'")
        .execute(pool)
        .await
        .expect("purges leftover test rows");
}

/// Four rows: two teams on one type, one of them repeated on a second type, and one row whose
/// `Data` is JSON `null`. Every filter in the tests below therefore has both a match and a
/// non-match to distinguish.
async fn seed(pool: &PgPool) {
    for (id, job_type, create_at, data) in [
        (
            "mmrsjob0000000000000000a1",
            TYPE,
            1788600000001_i64,
            serde_json::json!({"team_id": TEAM_A, "policy_id": TEAM_A}),
        ),
        (
            "mmrsjob0000000000000000a2",
            TYPE,
            1788600000003,
            serde_json::json!({"team_id": TEAM_A}),
        ),
        (
            "mmrsjob0000000000000000b1",
            TYPE,
            1788600000002,
            serde_json::json!({"team_id": TEAM_B}),
        ),
        (
            "mmrsjob0000000000000000c1",
            OTHER_TYPE,
            1788600000004,
            serde_json::json!({"team_id": TEAM_A}),
        ),
        (
            "mmrsjob0000000000000000d1",
            TYPE,
            1788600000005,
            serde_json::Value::Null,
        ),
    ] {
        sqlx::query(
            "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                               progress, data)
             VALUES ($1, $2, 0, $3, $3, $3, 'success', 100, $4)",
        )
        .bind(id)
        .bind(job_type)
        .bind(create_at)
        .bind(data)
        .execute(pool)
        .await
        .expect("the job row is written");
    }
}

/// The filter narrows by **both** the type and the data pair, and the compared value really is
/// compared — a different team is a different answer.
#[tokio::test]
async fn job_data_filter_narrows_by_type_and_by_value() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());

    let team_a = store
        .get_by_type_and_data(TYPE, "team_id", TEAM_A)
        .await
        .expect("queries");
    let mut ids: Vec<&str> = team_a.iter().map(|j| j.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["mmrsjob0000000000000000a1", "mmrsjob0000000000000000a2"],
        "team B's row and the other type's row are both excluded"
    );

    let team_b = store
        .get_by_type_and_data(TYPE, "team_id", TEAM_B)
        .await
        .expect("queries");
    assert_eq!(team_b.len(), 1, "the value is compared, not ignored");
    assert_eq!(team_b[0].id, "mmrsjob0000000000000000b1");

    // The *other* type holds a row with the same team, and must not appear above.
    let other = store
        .get_by_type_and_data(OTHER_TYPE, "team_id", TEAM_A)
        .await
        .expect("queries");
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].id, "mmrsjob0000000000000000c1");

    // A different key on the same rows: only the row that carries it.
    let by_policy = store
        .get_by_type_and_data(TYPE, "policy_id", TEAM_A)
        .await
        .expect("queries");
    assert_eq!(by_policy.len(), 1, "only one row has a policy_id");
    assert_eq!(by_policy[0].id, "mmrsjob0000000000000000a1");

    // A value nothing carries is empty rather than everything.
    let none = store
        .get_by_type_and_data(TYPE, "team_id", "mmrsjobteamzzzzzzzzzzzzzzz")
        .await
        .expect("queries");
    assert!(none.is_empty(), "an unmatched value matches nothing");

    // And the empty string — the shape a mutation that drops the value would send — must not
    // match the rows that *do* have a team_id.
    let empty = store
        .get_by_type_and_data(TYPE, "team_id", "")
        .await
        .expect("queries");
    assert!(empty.is_empty(), "an empty value is not a wildcard");

    purge(&pool).await;
}

/// A `Data` column holding JSON `null` decodes to `None` rather than failing the page, and the
/// type-page query orders newest first.
#[tokio::test]
async fn job_data_filter_type_page_orders_newest_first_and_tolerates_null_data() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());
    let page = store
        .get_all_by_type_page(TYPE, 0, 60)
        .await
        .expect("queries");

    let ours: Vec<&str> = page
        .iter()
        .filter(|j| j.id.starts_with("mmrsjob"))
        .map(|j| j.id.as_str())
        .collect();
    assert_eq!(
        ours,
        vec![
            "mmrsjob0000000000000000d1",
            "mmrsjob0000000000000000a2",
            "mmrsjob0000000000000000b1",
            "mmrsjob0000000000000000a1",
        ],
        "CreateAt DESC"
    );

    let null_row = page
        .iter()
        .find(|j| j.id == "mmrsjob0000000000000000d1")
        .expect("the null-data row is in the page");
    assert_eq!(null_row.data, None, "JSON null is Go's nil map");

    let populated = page
        .iter()
        .find(|j| j.id == "mmrsjob0000000000000000a2")
        .expect("present");
    assert_eq!(
        populated
            .data
            .as_ref()
            .and_then(|d| d.get("team_id"))
            .map(String::as_str),
        Some(TEAM_A)
    );

    purge(&pool).await;
}

/// `Get` finds one row and reports a miss as `NotFound` — the distinction the app layer turns
/// into 404 versus 500.
#[tokio::test]
async fn job_data_filter_get_finds_a_row_and_reports_a_miss() {
    if !db_enabled() {
        return;
    }
    let _fixtures = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());

    let found = store
        .get("mmrsjob0000000000000000a1")
        .await
        .expect("the row exists");
    assert_eq!(found.job_type, TYPE);
    assert_eq!(found.status, "success");
    assert_eq!(found.progress, 100);

    let err = store
        .get("mmrsjobzzzzzzzzzzzzzzzzzz")
        .await
        .expect_err("no such row");
    assert!(err.is_not_found(), "a miss is NotFound, not a driver error");

    purge(&pool).await;
}
