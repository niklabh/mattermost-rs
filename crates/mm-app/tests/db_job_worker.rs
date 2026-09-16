//! The job runtime's transitions and `DoJob`, against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_job_worker
//! ```
//!
//! # Why this is here and not in the parity suite
//!
//! No route reaches any of it. A client can create a job, cancel one and read one; nothing it can
//! send claims a job, records progress, or writes `Data["error"]`. The parity suite's job tests
//! therefore cannot tell a correct `ClaimJob` from one that always answers "somebody else took
//! it", which is precisely the failure that would leave every job pending for ever.
//!
//! # The job type is synthetic, and that is not tidiness — it is isolation
//!
//! The Go servers of the development stacks are running **against this same database**, with the
//! same watcher polling `Jobs` every fifteen seconds and the same `cleanup_desktop_tokens` worker.
//! A test that planted a pending job of a real type would be racing them for the claim, and one
//! that planted `DesktopTokens` rows older than five minutes would watch Go's worker delete them.
//!
//! So every job here is of a type **no Go worker is registered for**: Go's watcher looks it up,
//! gets a nil worker and skips the row. `IsValidJobType` rejects the string too, so no API route
//! can return it either. The one test that does exercise the real `cleanup_desktop_tokens` body
//! calls it directly through [`SimpleWorker::execute_body`] rather than through `DoJob`, and
//! plants rows the Go worker's own cut-off would not have taken.
//!
//! Every test is named `job_worker_*` so `MUTATE_FILTER` can select them by name.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mm_app::App;
use mm_app::job_runtime::{SimpleWorker, Workers, do_job};
use mm_model::job::{self, Job};
use mm_model::utils::{AppError, StringMap, get_millis};
use mm_store::{DesktopTokensStore, JobStore, SqlStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One mutable database, shared with the other `mm-app` suites.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Not in `AllJobTypes`, not registered by any Go worker. See the module note.
const SYNTHETIC_TYPE: &str = "mmrs_worker_probe";
const UNREGISTERED_TYPE: &str = "mmrs_worker_nobody";

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

fn app(pool: &PgPool) -> App {
    App::new(SqlStore::from_pool(pool.clone()))
}

async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM jobs WHERE id LIKE 'mmrsjw%'")
        .execute(pool)
        .await
        .expect("purges leftover job rows");
    sqlx::query("DELETE FROM desktoptokens WHERE token LIKE 'mmrsjw%'")
        .execute(pool)
        .await
        .expect("purges leftover token rows");
}

/// A pending job, written straight to the table so that `CreateJob`'s registry check — which
/// would refuse a synthetic type — is out of the way.
async fn plant(pool: &PgPool, id: &str, job_type: &str, create_at: i64) -> Job {
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                           progress, data)
         VALUES ($1, $2, 0, $3, 0, 0, 'pending', 0, $4)",
    )
    .bind(id)
    .bind(job_type)
    .bind(create_at)
    .bind(serde_json::json!({"planted": "yes"}))
    .execute(pool)
    .await
    .expect("the job row is written");

    Job {
        id: id.to_owned(),
        job_type: job_type.to_owned(),
        create_at,
        status: job::JOB_STATUS_PENDING.to_owned(),
        data: Some(StringMap::from([("planted".to_owned(), "yes".to_owned())])),
        ..Job::default()
    }
}

async fn read(pool: &PgPool, id: &str) -> Job {
    mm_store::SqlJobStore::new(pool.clone())
        .get(id)
        .await
        .expect("the row is there")
}

fn error_text(job: &Job) -> Option<&str> {
    job.data.as_ref()?.get("error").map(String::as_str)
}

/// A worker whose body is supplied per test. `runs` counts how many times it was entered, which
/// is how "the offer was dropped" is told from "the offer ran and did nothing".
/// The body runs **inside** the returned future, not while building it. `DoJob` spawns that
/// future, so a body that panics is a `JoinError` there rather than an unwind here — which is the
/// whole point of the panic test below.
fn probe_worker(
    runs: Arc<AtomicUsize>,
    body: impl Fn(App, Job) -> Result<(), String> + Send + Sync + 'static,
) -> SimpleWorker {
    let body = Arc::new(body);
    SimpleWorker::new(
        "MmrsProbe",
        SYNTHETIC_TYPE,
        |_config| true,
        move |app, job| {
            let runs = Arc::clone(&runs);
            let body = Arc::clone(&body);
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                body(app, job).map_err(|message| -> Box<dyn std::error::Error + Send + Sync> {
                    message.into()
                })
            })
        },
    )
}

/// `Workers` with one probe in it, and the slot the watcher would have handed a job to.
fn probe_registry(worker: SimpleWorker) -> Workers {
    let mut workers = Workers::new();
    workers.add(worker);
    workers
}

/// `ClaimJob` is one optimistic `UPDATE`, and that is the whole of the mutual exclusion between
/// this server and the Go server beside it: the first claim wins and the second is `Ok(None)`,
/// **not** an error.
///
/// It also stamps `StartAt`, which `UpdateOptimistically` — the method every later transition
/// uses — does not.
#[tokio::test]
async fn job_worker_claims_a_pending_job_exactly_once() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let job = plant(&pool, "mmrsjw0000000000000claim1", SYNTHETIC_TYPE, 1).await;
    let before = read(&pool, &job.id).await;
    assert_eq!(before.start_at, 0, "a pending job has not started");

    let claimed = app
        .claim_job(&job)
        .await
        .expect("the claim runs")
        .expect("the row was pending, so this claim wins");
    assert_eq!(claimed.status, job::JOB_STATUS_IN_PROGRESS);
    assert!(claimed.start_at > 0, "ClaimJob stamps StartAt");

    let again = app.claim_job(&job).await.expect("the second claim runs");
    assert!(
        again.is_none(),
        "the row is no longer pending: a lost race is Ok(None), not an error"
    );

    purge(&pool).await;
}

/// The success path end to end: claim, run the body, **progress 100**, then `success`.
///
/// The progress write is not decoration — it is a separate statement from the status write, and a
/// port that skipped it leaves a finished job reading 0%.
#[tokio::test]
async fn job_worker_runs_the_body_then_writes_progress_and_success() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    let workers = probe_registry(probe_worker(Arc::clone(&runs), |_app, _job| Ok(())));
    let slot = Arc::clone(workers.get(SYNTHETIC_TYPE).expect("registered"));

    let job = plant(&pool, "mmrsjw00000000000000000ok", SYNTHETIC_TYPE, 2).await;
    do_job(app, slot, job.clone()).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1, "the body ran once");
    let done = read(&pool, &job.id).await;
    assert_eq!(done.status, job::JOB_STATUS_SUCCESS);
    assert_eq!(done.progress, 100);
    assert!(done.start_at > 0);
    assert_eq!(
        error_text(&done),
        None,
        "a successful job carries no error key"
    );

    purge(&pool).await;
}

/// A failing body: `error`, **progress -1**, and `Data["error"]` built from the `AppError`.
///
/// The text is `app.job.error`, an em dash with a space on each side, then the body's own error —
/// the concatenation `SetJobError` performs. `app.job.error` rather than an English sentence
/// because [`AppError::message`] is the id until something translates it ([D-092]).
///
/// The planted `Data` key is **gone**: `UpdateOptimistically` writes the whole map.
#[tokio::test]
async fn job_worker_a_failing_body_is_recorded_as_an_error_with_progress_minus_one() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    let workers = probe_registry(probe_worker(Arc::clone(&runs), |_app, _job| {
        Err("the disk is on fire".to_owned())
    }));
    let slot = Arc::clone(workers.get(SYNTHETIC_TYPE).expect("registered"));

    let job = plant(&pool, "mmrsjw000000000000000err", SYNTHETIC_TYPE, 3).await;
    do_job(app, slot, job.clone()).await;

    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let done = read(&pool, &job.id).await;
    assert_eq!(done.status, job::JOB_STATUS_ERROR);
    assert_eq!(
        done.progress, -1,
        "a failed job is -1, not the last progress"
    );
    assert_eq!(
        error_text(&done),
        Some("app.job.error \u{2014} the disk is on fire"),
        "message, then U+2014 with a space either side, then the wrapped error"
    );
    assert_eq!(
        done.data
            .as_ref()
            .and_then(|d| d.get("planted"))
            .map(String::as_str),
        Some("yes"),
        "the key is added to the map the claimed job carried, not to a fresh one — \
         `job.Data[\"error\"] = …` on the struct the claim returned"
    );
    assert_eq!(done.data.as_ref().map(|d| d.len()), Some(2));

    purge(&pool).await;
}

/// A body that panics leaves the job in `error`, not wedged in `in_progress` for ever.
///
/// Go reaches the same row state through `HandleJobPanic` and then repanics the process; see
/// `do_job`'s doc comment for why the second half is not reproduced. The recorded text is `HandleJobPanic`'s
/// own `AppError`, which wraps nothing — so there is no em dash at all.
#[tokio::test]
async fn job_worker_a_panicking_body_still_lands_in_error() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    let workers = probe_registry(probe_worker(Arc::clone(&runs), |_app, _job| {
        panic!("a worker body that panics")
    }));
    let slot = Arc::clone(workers.get(SYNTHETIC_TYPE).expect("registered"));

    let job = plant(&pool, "mmrsjw00000000000000panic", SYNTHETIC_TYPE, 4).await;
    do_job(app, Arc::clone(&slot), job.clone()).await;

    let done = read(&pool, &job.id).await;
    assert_eq!(done.status, job::JOB_STATUS_ERROR);
    assert_eq!(done.progress, -1);
    assert_eq!(
        error_text(&done),
        Some("app.job.update.app_error"),
        "HandleJobPanic's own error, with nothing wrapped and so no separator"
    );

    // The slot is released, so the next poll can hand this worker another job.
    assert!(!slot.is_busy());

    purge(&pool).await;
}

/// `SetJobError`'s **second attempt**. A job cancelled while it was running is
/// `cancel_requested`, so the first optimistic write — guarded on `in_progress` — matches
/// nothing, and the failure is recorded by the retry.
///
/// Without the retry the error is lost silently and the row stays `cancel_requested` for ever.
#[tokio::test]
async fn job_worker_an_error_on_a_cancel_requested_job_is_recorded_by_the_second_attempt() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    // The body moves its own row to `cancel_requested` — what `POST /jobs/{id}/cancel` does to a
    // running job — and then fails. Written as its own worker rather than through `probe_worker`
    // because the update has to be awaited inside the body.
    let moved = pool.clone();
    let worker = SimpleWorker::new("MmrsProbe", SYNTHETIC_TYPE, |_config| true, {
        let runs = Arc::clone(&runs);
        move |_app, job: Job| {
            let runs = Arc::clone(&runs);
            let pool = moved.clone();
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                sqlx::query("UPDATE jobs SET status = 'cancel_requested' WHERE id = $1")
                    .bind(&job.id)
                    .execute(&pool)
                    .await
                    .expect("the cancellation lands");
                Err::<(), Box<dyn std::error::Error + Send + Sync>>("stopped on request".into())
            })
        }
    });
    let workers = probe_registry(worker);
    let slot = Arc::clone(workers.get(SYNTHETIC_TYPE).expect("registered"));

    let job = plant(&pool, "mmrsjw0000000000000cancel", SYNTHETIC_TYPE, 5).await;
    do_job(app, slot, job.clone()).await;

    let done = read(&pool, &job.id).await;
    assert_eq!(
        done.status,
        job::JOB_STATUS_ERROR,
        "the retry guarded on cancel_requested and landed"
    );
    assert_eq!(
        error_text(&done),
        Some("app.job.error \u{2014} stopped on request")
    );

    purge(&pool).await;
}

/// Neither guard matches — the row is already terminal — so both attempts miss and the whole
/// transition fails with `jobs.set_job_error.update.error`. `DoJob` logs that and moves on, and
/// the row keeps the status somebody else gave it.
#[tokio::test]
async fn job_worker_an_error_on_a_terminal_job_changes_nothing() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let mut job = plant(&pool, "mmrsjw000000000000terminal", SYNTHETIC_TYPE, 6).await;
    sqlx::query("UPDATE jobs SET status = 'canceled' WHERE id = $1")
        .bind(&job.id)
        .execute(&pool)
        .await
        .expect("the row is terminal");

    let failure = AppError::new("DoJob", "app.job.error", None, String::new(), 500);
    let err = app
        .set_job_error(&mut job, Some(&failure))
        .await
        .expect_err("neither guard matches");
    assert_eq!(err.id, "jobs.set_job_error.update.error");
    assert_eq!(err.status_code, 500);
    assert_eq!(err.detailed_error, format!("id={}", job.id));

    let after = read(&pool, &job.id).await;
    assert_eq!(after.status, "canceled", "the row is untouched");
    assert_eq!(error_text(&after), None);

    purge(&pool).await;
}

/// `SetJobError` with **no** `AppError` is the short branch: an unconditional status write, and
/// nothing said about `Data` or `Progress`.
#[tokio::test]
async fn job_worker_an_error_with_no_app_error_only_writes_the_status() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let mut job = plant(&pool, "mmrsjw00000000000bareerr", SYNTHETIC_TYPE, 7).await;
    app.set_job_error(&mut job, None)
        .await
        .expect("the unconditional write succeeds whatever the status");

    let after = read(&pool, &job.id).await;
    assert_eq!(after.status, job::JOB_STATUS_ERROR);
    assert_eq!(after.progress, 0, "Progress is not touched by this branch");
    assert_eq!(
        after.data.as_ref().and_then(|d| d.get("planted")),
        Some(&"yes".to_owned()),
        "and neither is Data"
    );

    purge(&pool).await;
}

/// The watcher's dispatch decisions, all three of them, in one poll.
///
/// A type with no worker is skipped; a worker that is already running a job drops the offer
/// rather than queueing it; and what is left is dispatched oldest-first.
#[tokio::test]
async fn job_worker_poll_and_notify_skips_unknown_types_and_busy_workers() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    // A body that blocks until released, so the slot is observably busy for the second job.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let held = Arc::clone(&gate);
    let worker = SimpleWorker::new("MmrsProbe", SYNTHETIC_TYPE, |_config| true, {
        let runs = Arc::clone(&runs);
        move |_app, _job| {
            runs.fetch_add(1, Ordering::SeqCst);
            let held = Arc::clone(&held);
            Box::pin(async move {
                let _permit = held.acquire().await.expect("the gate is open");
                Ok(())
            })
        }
    });
    let workers = Arc::new(probe_registry(worker));

    // Two jobs this worker could take and one nobody can.
    plant(&pool, "mmrsjw0000000000000poll_1", SYNTHETIC_TYPE, 10).await;
    plant(&pool, "mmrsjw0000000000000poll_2", SYNTHETIC_TYPE, 11).await;
    plant(&pool, "mmrsjw0000000000000poll_3", UNREGISTERED_TYPE, 12).await;

    let dispatched = app.poll_and_notify(&workers).await;
    assert_eq!(
        dispatched, 1,
        "one worker takes one job: the second offer is dropped and the third has no worker"
    );

    // The one that ran is the **oldest**, which is what the ascending order buys.
    let mut claimed = None;
    for _ in 0..200 {
        let first = read(&pool, "mmrsjw0000000000000poll_1").await;
        if first.status == job::JOB_STATUS_IN_PROGRESS {
            claimed = Some(first);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let claimed = claimed.expect("the oldest pending job was claimed");
    assert_eq!(claimed.id, "mmrsjw0000000000000poll_1");

    assert_eq!(
        read(&pool, "mmrsjw0000000000000poll_2").await.status,
        job::JOB_STATUS_PENDING,
        "the dropped offer leaves the job pending for the next poll"
    );
    assert_eq!(
        read(&pool, "mmrsjw0000000000000poll_3").await.status,
        job::JOB_STATUS_PENDING,
        "a type with no worker is never claimed"
    );

    gate.add_permits(1);
    for _ in 0..200 {
        if read(&pool, "mmrsjw0000000000000poll_1").await.status == job::JOB_STATUS_SUCCESS {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        read(&pool, "mmrsjw0000000000000poll_1").await.status,
        job::JOB_STATUS_SUCCESS
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    purge(&pool).await;
}

/// A disabled worker is never offered a job — Go's equivalent is that it has no goroutine parked
/// on its channel, so the `default:` arm fires.
#[tokio::test]
async fn job_worker_a_disabled_worker_is_never_offered_a_job() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    let runs = Arc::new(AtomicUsize::new(0));
    let worker = SimpleWorker::new("MmrsProbe", SYNTHETIC_TYPE, |_config| false, {
        let runs = Arc::clone(&runs);
        move |_app, _job| {
            runs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    });
    let workers = Arc::new(probe_registry(worker));

    plant(&pool, "mmrsjw000000000000disabled", SYNTHETIC_TYPE, 20).await;
    assert_eq!(app.poll_and_notify(&workers).await, 0);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(
        read(&pool, "mmrsjw000000000000disabled").await.status,
        job::JOB_STATUS_PENDING
    );

    purge(&pool).await;
}

/// The scheduler's two reads. `CheckForPendingJobsByType` is a count reduced to a bool, and
/// `GetLastSuccessfulJobByType` turns `ErrNotFound` into `Ok(None)` — a type that has never run
/// is the ordinary case on a new deployment, and a scheduler that treated it as an error would
/// never schedule anything.
#[tokio::test]
async fn job_worker_the_schedulers_two_reads_answer_for_a_type_that_has_never_run() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    assert!(
        !app.check_for_pending_jobs_by_type(SYNTHETIC_TYPE)
            .await
            .expect("counts"),
        "nothing pending yet"
    );
    assert!(
        app.get_last_successful_job_by_type(SYNTHETIC_TYPE)
            .await
            .expect("a missing row is not an error")
            .is_none(),
        "ErrNotFound is Ok(None)"
    );

    plant(&pool, "mmrsjw00000000000000sched", SYNTHETIC_TYPE, 30).await;
    assert!(
        app.check_for_pending_jobs_by_type(SYNTHETIC_TYPE)
            .await
            .expect("counts"),
        "one pending row is enough"
    );
    // A pending job is not a successful one.
    assert!(
        app.get_last_successful_job_by_type(SYNTHETIC_TYPE)
            .await
            .expect("reads")
            .is_none()
    );
    // And a different type does not see it.
    assert!(
        !app.check_for_pending_jobs_by_type(UNREGISTERED_TYPE)
            .await
            .expect("counts")
    );

    sqlx::query("UPDATE jobs SET status = 'success' WHERE id = $1")
        .bind("mmrsjw00000000000000sched")
        .execute(&pool)
        .await
        .expect("the row succeeds");
    let last = app
        .get_last_successful_job_by_type(SYNTHETIC_TYPE)
        .await
        .expect("reads")
        .expect("there is one now");
    assert_eq!(last.id, "mmrsjw00000000000000sched");
    assert!(
        !app.check_for_pending_jobs_by_type(SYNTHETIC_TYPE)
            .await
            .expect("counts"),
        "and it is no longer pending"
    );

    purge(&pool).await;
}

/// The real `cleanup_desktop_tokens` body: every `DesktopTokens` row older than five minutes.
///
/// Called through [`SimpleWorker::execute_body`] rather than `DoJob` — see the module note on why
/// nothing here goes near the shared claim. The rows are planted **around the worker's own
/// cut-off**, computed from the same clock the body uses, so the boundary is what is asserted
/// rather than "old rows go". The surviving row is minutes in the future, which also puts it out
/// of reach of the Go server's copy of this worker.
#[tokio::test]
async fn job_worker_the_cleanup_desktop_tokens_body_deletes_only_rows_past_its_cutoff() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = app(&pool);

    // The worker's own arithmetic: seconds, five minutes back.
    let cutoff = get_millis() / 1000 - 5 * 60;
    for (token, create_at) in [
        ("mmrsjw_old", cutoff - 1),
        ("mmrsjw_boundary", cutoff),
        // Inside the five-minute window. Without it the cut-off's *width* is untested: a worker
        // with `maxAge = 0` deletes exactly the same two rows as one with five minutes, because
        // every other row here is on the far side of both.
        ("mmrsjw_recent", cutoff + 4 * 60),
        ("mmrsjw_future", cutoff + 3600),
    ] {
        sqlx::query("INSERT INTO desktoptokens (token, createat, userid) VALUES ($1, $2, $3)")
            .bind(token)
            .bind(create_at)
            .bind("mmrsjwuserxxxxxxxxxxxxxxxx")
            .execute(&pool)
            .await
            .expect("the token row is written");
    }

    let worker = mm_app::job_runtime::cleanup_desktop_tokens_worker();
    assert_eq!(worker.job_type, job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS);
    worker
        .execute_body(app.clone(), Job::default())
        .await
        .expect("the delete runs");

    let left: Vec<String> =
        sqlx::query_scalar("SELECT token FROM desktoptokens WHERE token LIKE 'mmrsjw%'")
            .fetch_all(&pool)
            .await
            .expect("reads back");
    assert!(
        !left.contains(&"mmrsjw_old".to_owned()),
        "a row a second past the cut-off is deleted"
    );
    assert!(
        left.contains(&"mmrsjw_recent".to_owned()),
        "a row a minute inside the five-minute window is kept — this is what makes maxAge's \
         value, and not merely its sign, observable"
    );
    assert!(
        left.contains(&"mmrsjw_future".to_owned()),
        "a row inside the five-minute window is kept"
    );
    assert!(
        left.contains(&"mmrsjw_boundary".to_owned()),
        "strictly less-than: the row exactly on the cut-off stays"
    );

    // The store's `delete` is unrelated to the age filter and still works on what is left.
    app.store()
        .desktop_tokens()
        .delete("mmrsjw_future")
        .await
        .expect("deletes");

    purge(&pool).await;
}
