//! The four `JobStore` methods the job **runtime** needs, plus `DesktopTokens.DeleteOlderThan`,
//! against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_job_store_runtime
//! ```
//!
//! # Why these are here and not in the parity suite
//!
//! None of them is reachable from a route. `GetAllByStatus` is the watcher's poll,
//! `GetCountByStatusAndType` and `GetNewestJobByStatusesAndType` are the scheduler's two reads,
//! `UpdateOptimistically` is how a running job records progress and failure, and `DeleteOlderThan`
//! is the whole of the `cleanup_desktop_tokens` job. A REST request reaches none of them, so the
//! only place their SQL can be held to account is here, with rows planted directly.
//!
//! # Every test is named `job_runtime_*` on purpose
//!
//! `MUTATE_FILTER` filters test **names**, not test targets: a filter of this file's name matches
//! no test function, cargo runs zero tests and exits 0, and every mutation is reported SURVIVED.
//! That is the trap `db_job_data_filter.rs` fell into; the shared prefix is the fix.
//!
//! # The fixtures discriminate on purpose
//!
//! Three things here are only visible against rows chosen to expose them, and each one is a
//! mutation that would otherwise survive:
//!
//! - **`ORDER BY CreateAt ASC`.** The seeded rows are inserted out of order and have distinct
//!   `CreateAt`s, so `DESC` reverses the answer instead of returning the same list.
//! - **`UpdateOptimistically` does not write `StartAt`.** The seeded `StartAt` differs from both
//!   `CreateAt` and `LastActivityAt`, so a statement that set it would move a value the test can
//!   see.
//! - **`DeleteOlderThan` is strictly less-than.** One row sits exactly *on* the cut-off.

use mm_model::job::Job;
use mm_model::utils::StringMap;
use mm_store::{DesktopTokensStore, JobStore, SqlDesktopTokensStore, SqlJobStore, StoreError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// These tests seed and purge one shared prefix, so two running at once delete each other's rows
/// mid-assertion. Serialised for the reason `db_job_data_filter.rs` is.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// **Deliberately not a real job type**, for the reason `db_job_data_filter.rs` gives: a row of a
/// real type left behind by a panicking test is visible to `getJobs`, which the parity suite
/// asserts against. `IsValidJobType` rejects this string, so no route can ever return it.
const TYPE: &str = "mmrs_runtime_a";
const OTHER_TYPE: &str = "mmrs_runtime_b";

const PENDING_OLDEST: &str = "mmrsjrt00000000000000000p1";
const PENDING_MIDDLE: &str = "mmrsjrt00000000000000000p2";
const PENDING_NEWEST: &str = "mmrsjrt00000000000000000p3";
const RUNNING: &str = "mmrsjrt00000000000000000r1";
const SUCCEEDED_OLD: &str = "mmrsjrt00000000000000000s1";
const SUCCEEDED_NEW: &str = "mmrsjrt00000000000000000s2";
const WARNED: &str = "mmrsjrt00000000000000000w1";

/// A base far enough in the past that no real row shares it, and far enough apart that the
/// ordering assertions cannot be satisfied by accident.
const BASE: i64 = 1_700_000_000_000;

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
    sqlx::query("DELETE FROM jobs WHERE id LIKE 'mmrsjrt%'")
        .execute(pool)
        .await
        .expect("purges leftover job rows");
    sqlx::query("DELETE FROM desktoptokens WHERE token LIKE 'mmrsjrt%'")
        .execute(pool)
        .await
        .expect("purges leftover token rows");
}

/// `CreateAt`, `StartAt` and `LastActivityAt` are given **three different values** per row so
/// that a query reading the wrong column is a different answer rather than the same one.
#[allow(clippy::too_many_arguments)]
async fn seed(pool: &PgPool) {
    for (id, job_type, status, create_at, progress) in [
        // Inserted newest-first so that insertion order cannot stand in for `ORDER BY`.
        (PENDING_NEWEST, TYPE, "pending", BASE + 300, 0_i64),
        (PENDING_OLDEST, TYPE, "pending", BASE + 100, 0),
        (PENDING_MIDDLE, OTHER_TYPE, "pending", BASE + 200, 0),
        (RUNNING, TYPE, "in_progress", BASE + 400, 42),
        (SUCCEEDED_OLD, TYPE, "success", BASE + 10, 100),
        (SUCCEEDED_NEW, TYPE, "success", BASE + 20, 100),
        (WARNED, TYPE, "warning", BASE + 30, 100),
    ] {
        sqlx::query(
            "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                               progress, data)
             VALUES ($1, $2, 0, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(job_type)
        .bind(create_at)
        // StartAt and LastActivityAt are deliberately distinct from CreateAt and from each other.
        .bind(create_at + 1)
        .bind(create_at + 2)
        .bind(status)
        .bind(progress)
        .bind(serde_json::json!({"seed": "yes"}))
        .execute(pool)
        .await
        .expect("the job row is written");
    }
}

async fn row(pool: &PgPool, id: &str) -> (i64, i64, i64, String, i64) {
    sqlx::query_as::<_, (i64, i64, i64, String, i64)>(
        "SELECT createat, startat, lastactivityat, status, progress FROM jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the row is there")
}

/// The watcher's poll: **every** pending job, of every type, oldest first.
///
/// `DESC` — which is what every other job read uses — reverses this list. The three seeded
/// pending rows have distinct `CreateAt`s and were inserted in a third order, so neither the
/// wrong direction nor "no ordering at all" can pass.
#[tokio::test]
async fn job_runtime_pending_jobs_come_back_oldest_first() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());
    let jobs = store
        .get_all_by_status("pending")
        .await
        .expect("the pending jobs are read");

    let ours: Vec<&str> = jobs
        .iter()
        .filter(|j| j.id.starts_with("mmrsjrt"))
        .map(|j| j.id.as_str())
        .collect();
    assert_eq!(
        ours,
        vec![PENDING_OLDEST, PENDING_MIDDLE, PENDING_NEWEST],
        "GetAllByStatus is ORDER BY CreateAt ASC"
    );

    // And it is not filtered by type — the middle row is a different type and is still there.
    assert!(jobs.iter().any(|j| j.job_type == OTHER_TYPE));
    // Nothing that is not pending comes back.
    assert!(!jobs.iter().any(|j| j.id == RUNNING));

    purge(&pool).await;
}

/// `GetCountByStatusAndType` narrows on **both**, and counts rather than fetching.
#[tokio::test]
async fn job_runtime_the_pending_count_narrows_by_status_and_type() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());
    assert_eq!(
        store
            .get_count_by_status_and_type("pending", TYPE)
            .await
            .expect("counts"),
        2,
        "two of the three pending rows are this type"
    );
    assert_eq!(
        store
            .get_count_by_status_and_type("pending", OTHER_TYPE)
            .await
            .expect("counts"),
        1
    );
    // The status half is real: this type has four non-pending rows and none of them counts.
    assert_eq!(
        store
            .get_count_by_status_and_type("canceled", TYPE)
            .await
            .expect("counts"),
        0
    );

    purge(&pool).await;
}

/// The plural lookup, and the `message_export` reason it exists: `warning` is a status a caller
/// may want counted as a success, and it is only reachable through the slice form.
#[tokio::test]
async fn job_runtime_the_newest_job_can_be_selected_across_several_statuses() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());

    // `success` alone: the newer of the two successes, by CreateAt DESC.
    let newest = store
        .get_newest_job_by_statuses_and_type(&["success".to_owned()], TYPE)
        .await
        .expect("finds one");
    assert_eq!(newest.id, SUCCEEDED_NEW);

    // Adding `warning` changes the answer, because the warned row is newer than both successes.
    let newest = store
        .get_newest_job_by_statuses_and_type(&["warning".to_owned(), "success".to_owned()], TYPE)
        .await
        .expect("finds one");
    assert_eq!(
        newest.id, WARNED,
        "the warning row is the newest of the union"
    );

    // The singular method is the one-element call, and answers the same as the slice form.
    let singular = store
        .get_newest_job_by_status_and_type("success", TYPE)
        .await
        .expect("finds one");
    assert_eq!(singular.id, SUCCEEDED_NEW);

    // A type with no such row is NotFound, which the app layer turns into `Ok(None)`.
    let missing = store
        .get_newest_job_by_statuses_and_type(&["success".to_owned()], OTHER_TYPE)
        .await;
    assert!(matches!(missing, Err(StoreError::NotFound { .. })));

    purge(&pool).await;
}

/// `UpdateOptimistically` writes four columns and **leaves `StartAt` alone**.
///
/// The seeded `StartAt` is `CreateAt + 1`, distinct from everything else on the row, so a
/// statement that also stamped it — which is what `UpdateStatusOptimistically` does — moves a
/// value this assertion reads.
#[tokio::test]
async fn job_runtime_an_optimistic_update_writes_data_and_progress_but_not_start_at() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());
    let (create_at, start_at, last_activity_at, _, progress) = row(&pool, RUNNING).await;
    assert_eq!(start_at, create_at + 1);
    assert_eq!(progress, 42);

    let mut data = StringMap::new();
    data.insert("error".to_owned(), "something went wrong".to_owned());
    let job = Job {
        id: RUNNING.to_owned(),
        job_type: TYPE.to_owned(),
        status: "error".to_owned(),
        progress: -1,
        data: Some(data),
        ..Job::default()
    };

    let updated = store
        .update_optimistically(&job, "in_progress")
        .await
        .expect("the update runs")
        .expect("the guard matched");
    assert_eq!(updated.status, "error");
    assert_eq!(updated.progress, -1);
    assert_eq!(
        updated
            .data
            .as_ref()
            .and_then(|d| d.get("error"))
            .map(String::as_str),
        Some("something went wrong"),
        "the whole Data map is replaced, not merged — the seeded key is gone"
    );
    assert_eq!(updated.data.as_ref().map(|d| d.len()), Some(1));

    let (create_at_after, start_at_after, last_activity_after, status, progress_after) =
        row(&pool, RUNNING).await;
    assert_eq!(create_at_after, create_at, "CreateAt is never written");
    assert_eq!(start_at_after, start_at, "StartAt is NOT in the SET list");
    assert!(
        last_activity_after > last_activity_at,
        "LastActivityAt is stamped with the current clock"
    );
    assert_eq!(status, "error");
    assert_eq!(progress_after, -1);

    purge(&pool).await;
}

/// The guard really guards: a second attempt against the status the row no longer holds matches
/// nothing and is `Ok(None)`, not an error. That `None` is the whole of `SetJobError`'s second
/// attempt and of `SetJobProgress`'s silent no-op on a cancelled job.
#[tokio::test]
async fn job_runtime_a_missed_optimistic_guard_is_none_and_writes_nothing() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;

    let store = SqlJobStore::new(pool.clone());
    let before = row(&pool, RUNNING).await;

    let job = Job {
        id: RUNNING.to_owned(),
        job_type: TYPE.to_owned(),
        status: "success".to_owned(),
        progress: 100,
        data: None,
        ..Job::default()
    };
    // The row is `in_progress`; guarding on `cancel_requested` matches nothing.
    let missed = store
        .update_optimistically(&job, "cancel_requested")
        .await
        .expect("the statement runs");
    assert!(missed.is_none(), "no row matched is Ok(None)");
    assert_eq!(row(&pool, RUNNING).await, before, "and nothing was written");

    purge(&pool).await;
}

/// `DeleteOlderThan` is `sq.Lt` — **strictly** less than. A row exactly on the cut-off survives.
#[tokio::test]
async fn job_runtime_desktop_tokens_older_than_the_cutoff_go_and_the_boundary_row_stays() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    // `CreateAt` here is Unix **seconds**, not millis — see the store's doc comment.
    const CUTOFF: i64 = 1_700_000_000;
    for (token, create_at) in [
        ("mmrsjrt_token_older", CUTOFF - 1),
        ("mmrsjrt_token_on_the_cutoff", CUTOFF),
        ("mmrsjrt_token_newer", CUTOFF + 1),
    ] {
        sqlx::query("INSERT INTO desktoptokens (token, createat, userid) VALUES ($1, $2, $3)")
            .bind(token)
            .bind(create_at)
            .bind("mmrsjrtuserxxxxxxxxxxxxxxx")
            .execute(&pool)
            .await
            .expect("the token row is written");
    }

    let store = SqlDesktopTokensStore::new(pool.clone());
    store
        .delete_older_than(CUTOFF)
        .await
        .expect("the delete runs");

    let left: Vec<String> = sqlx::query_scalar(
        "SELECT token FROM desktoptokens WHERE token LIKE 'mmrsjrt%' ORDER BY createat",
    )
    .fetch_all(&pool)
    .await
    .expect("reads back");
    assert_eq!(
        left,
        vec![
            "mmrsjrt_token_on_the_cutoff".to_owned(),
            "mmrsjrt_token_newer".to_owned()
        ],
        "strictly less-than: the boundary row is kept"
    );

    // Deleting again is not an error and takes nothing else.
    store
        .delete_older_than(CUTOFF)
        .await
        .expect("a second delete is a no-op");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM desktoptokens WHERE token LIKE 'mmrsjrt%'")
            .fetch_one(&pool)
            .await
            .expect("counts");
    assert_eq!(count, 2);

    purge(&pool).await;
}
