//! Port of the job **runtime** — `jobs/jobs.go`'s lifecycle half, `jobs/base_workers.go` and
//! `jobs/jobs_watcher.go`. This is the half that runs a job, as opposed to `crate::job`, which is
//! the half the REST routes call.
//!
//! ```text
//!   POST /api/v4/jobs ──▶ Jobs row, status=pending
//!                              │
//!            Watcher, every 15s: GetAllByStatus(pending), oldest first
//!                              │
//!            Workers.get(job.Type) ──▶ idle? ──▶ SimpleWorker.DoJob
//!                                                    │
//!                        ClaimJob (pending → in_progress, optimistic)
//!                                                    │
//!                              execute ──┬── Ok  ──▶ progress 100, status=success
//!                                        └── Err ──▶ status=error, Data["error"]=…
//! ```
//!
//! # Every transition is optimistic, and that is what makes two servers safe
//!
//! `ClaimJob` is `UPDATE … WHERE Id = ? AND Status = 'pending'`. Mattermost runs this same loop
//! on **every node of a cluster** against one `Jobs` table, and the `WHERE` clause is the entire
//! mutual exclusion: the node whose `UPDATE` matches gets the job and every other node gets zero
//! rows and moves on. That property is load-bearing here for a different reason — the Go server
//! this one forwards to is polling the same table with the same statement. Running both is a
//! race that is *decided*, not a corruption.
//!
//! Schedulers are the opposite; see [`crate::job_scheduler`] for why two of those must not run.
//!
//! # A goroutine parked on a channel is a worker that is idle
//!
//! Go gives each worker an **unbuffered** `jobs chan model.Job` and the watcher offers into it
//! with a `default:` arm:
//!
//! ```go
//! select {
//! case worker.JobChannel() <- *job:
//! default:
//! }
//! ```
//!
//! An unbuffered send succeeds only when a receiver is already parked, so the offer lands exactly
//! when the worker is between jobs, and is **dropped** otherwise — a busy worker's pending jobs
//! are not queued, they are re-offered on the next poll. That is why the queue is not a queue and
//! why [`JobStore::get_all_by_status`] is ordered oldest-first: without it, a backlog would starve
//! its own head.
//!
//! This port drops the channel and keeps the property: [`WorkerSlot::busy`] is set for exactly as
//! long as Go's goroutine would be inside `DoJob`, and an offer to a busy slot is discarded. A
//! `tokio::mpsc` of capacity one would have been the obvious translation and is the wrong one —
//! it buffers, so a stopped or wedged worker accumulates a job that never runs.
//!
//! # No metrics
//!
//! `IncrementJobActive`/`DecrementJobActive` are `einterfaces.MetricsInterface`, nil on every
//! build from this tree, so each of the calls Go makes is a no-op there and absent here.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mm_model::job::{self, Job};
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::{DesktopTokensStore, JobStore, StoreError};

use crate::App;
use crate::config::Config;

/// Go's `jobs.DefaultWatcherPollingInterval` (jobs_watcher.go:16), in milliseconds.
///
/// A `var` rather than a `const` upstream, with the comment "Defining as `var` rather than
/// `const` allows tests to lower the interval" — [`Watcher::new`] takes it as an argument for the
/// same reason.
pub const DEFAULT_WATCHER_POLLING_INTERVAL_MS: u64 = 15_000;

/// The error a worker body returns — Go's plain `error`, not an `AppError`.
///
/// `DoJob` is what turns it into one, by wrapping it in `app.job.error`; the worker itself never
/// builds an `AppError`, and `SetJobError`'s `Data["error"]` therefore always begins with that
/// id and carries the worker's own text after an em dash. See [`App::set_job_error`].
pub type WorkerError = Box<dyn std::error::Error + Send + Sync>;

/// The future a worker body returns. Owned arguments, so it is `'static` and can be spawned —
/// which is how a panic inside a job is caught. See [`do_job`].
pub type ExecuteFuture = Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send>>;

/// `Box<dyn Error>` is not itself an `Error`, so wrapping a worker failure into an `AppError`
/// needs a named type. It exists for [`AppError::wrap`] and carries nothing of its own.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ExecuteError(WorkerError);

/// Port of `jobs.SimpleWorker` (base_workers.go:13) — minus the channel, per the module note.
///
/// Go's struct also holds a `name` separate from the job type (`"CleanupDesktopTokens"` against
/// `cleanup_desktop_tokens`); it appears only in log annotations, and is kept for the same
/// reason.
pub struct SimpleWorker {
    /// `worker_name` in Go's log fields — not the job type.
    pub name: &'static str,
    /// The `Jobs.Type` this worker claims. The registry's key, and what `Workers.Get` matches.
    pub job_type: &'static str,
    /// Port of `isEnabled func(cfg *model.Config) bool`.
    is_enabled: fn(&Config) -> bool,
    /// Port of `execute func(logger mlog.LoggerIFace, job *model.Job) error`.
    execute: Arc<dyn Fn(App, Job) -> ExecuteFuture + Send + Sync>,
}

impl std::fmt::Debug for SimpleWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleWorker")
            .field("name", &self.name)
            .field("job_type", &self.job_type)
            .finish_non_exhaustive()
    }
}

impl SimpleWorker {
    /// Port of `jobs.NewSimpleWorker` (base_workers.go:25).
    pub fn new(
        name: &'static str,
        job_type: &'static str,
        is_enabled: fn(&Config) -> bool,
        execute: impl Fn(App, Job) -> ExecuteFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            job_type,
            is_enabled,
            execute: Arc::new(execute),
        }
    }

    /// Port of `(*SimpleWorker).IsEnabled` (base_workers.go:65).
    pub fn is_enabled(&self, config: &Config) -> bool {
        (self.is_enabled)(config)
    }

    /// The worker's body alone, with none of [`do_job`]'s claim-and-record wrapper around it.
    ///
    /// Nothing in the server calls this — `DoJob` is the only production path. It exists so that
    /// a test can exercise a worker's body against the shared database **without racing the Go
    /// server for the claim**: both servers poll the same `Jobs` table, so a test that went
    /// through `DoJob` would sometimes find its job already `in_progress` elsewhere.
    pub fn execute_body(&self, app: App, job: Job) -> ExecuteFuture {
        (self.execute)(app, job)
    }
}

/// A registered worker and the one bit of state Go keeps in its scheduler: whether the goroutine
/// is inside `DoJob`.
#[derive(Debug)]
pub struct WorkerSlot {
    worker: SimpleWorker,
    /// `false` is Go's "parked on `<-worker.jobs`", the only state in which an offer lands.
    busy: AtomicBool,
}

impl WorkerSlot {
    pub fn worker(&self) -> &SimpleWorker {
        &self.worker
    }

    /// Port of the watcher's `select { case ch <- job: default: }`: `true` when the offer was
    /// taken, `false` when it was dropped because the worker was mid-job.
    ///
    /// Taking the offer marks the slot busy; [`WorkerSlot::release`] is the return from `DoJob`.
    /// `compare_exchange` rather than a load-then-store because two watcher polls can overlap:
    /// a poll that takes longer than the interval would otherwise hand the same worker two jobs.
    fn take(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release(&self) {
        self.busy.store(false, Ordering::Release);
    }

    /// Whether the slot is currently running a job. Exposed for tests and for the shutdown path.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

/// Port of `jobs.Workers` (workers.go:14) — the `map[string]model.Worker` and nothing else.
///
/// Go's struct also carries `ConfigService`, the `Watcher`, a config-listener id and a `running`
/// flag. The listener exists to start and stop worker goroutines when a setting flips
/// (`handleConfigChange`, workers.go:61); there are no goroutines to start here, so the same
/// effect comes from [`SimpleWorker::is_enabled`] being consulted at offer time. The consequence
/// is the one Go's design has too: a worker disabled while a job is running finishes that job.
#[derive(Debug, Default)]
pub struct Workers {
    workers: HashMap<&'static str, Arc<WorkerSlot>>,
}

impl Workers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Port of `(*Workers).AddWorker` (workers.go:36). Keyed by job type, as Go's callers key it:
    /// `RegisterJobType(model.JobTypeX, worker, scheduler)`.
    pub fn add(&mut self, worker: SimpleWorker) {
        self.workers.insert(
            worker.job_type,
            Arc::new(WorkerSlot {
                worker,
                busy: AtomicBool::new(false),
            }),
        );
    }

    /// Port of `(*Workers).Get` (workers.go:40). A map miss is Go's nil interface, which is what
    /// `_createJob` turns into `model.job.is_valid.type.app_error` and what the watcher skips on.
    pub fn get(&self, job_type: &str) -> Option<&Arc<WorkerSlot>> {
        self.workers.get(job_type)
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// JobServer: the lifecycle transitions a running job makes
// ---------------------------------------------------------------------------

impl App {
    /// Port of `JobServer.ClaimJob` (jobs/jobs.go:110).
    ///
    /// `Ok(None)` is Go's nil job with a nil error: the row was no longer `pending`, so somebody
    /// else — another node, or the Go server beside this one — claimed it first. That is the
    /// expected outcome of a race, not a failure, and `DoJob` returns silently on it.
    ///
    /// The `job_updated` event is published **only on a successful claim**, so a client watching
    /// a cluster sees one `in_progress` for a job, not one per node.
    #[tracing::instrument(skip_all, fields(job_id = %job.id, job_type = %job.job_type, claimed))]
    pub async fn claim_job(&self, job: &Job) -> AppResult<Option<Job>> {
        let claimed = self
            .store()
            .job()
            .update_status_optimistically(
                &job.id,
                job::JOB_STATUS_PENDING,
                job::JOB_STATUS_IN_PROGRESS,
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, job_id = %job.id, "claim failed");
                AppError::boxed(
                    "ClaimJob",
                    "app.job.update.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("claimed", claimed.is_some());
        if let Some(claimed) = &claimed {
            self.publish_job_status(claimed, job::JOB_STATUS_IN_PROGRESS)
                .await;
        }
        Ok(claimed)
    }

    /// Port of `JobServer.SetJobProgress` (jobs/jobs.go:142).
    ///
    /// **It mutates the caller's job** — status to `in_progress` and the new progress — before
    /// writing, because `UpdateOptimistically` takes the whole struct and writes `Status`, `Data`
    /// and `Progress` from it. A caller that kept its own copy would go on to write a stale
    /// progress on its next call, which is why `job` is `&mut` here rather than `&`.
    ///
    /// The guard is `in_progress`: a job that has since been cancelled matches no row, and Go
    /// treats that as **success** with no event — `ret == nil` skips the publish and returns
    /// `nil`. So a cancelled job's worker keeps running and its progress writes quietly go
    /// nowhere; only the terminal `SetJobError` notices, through its second attempt.
    #[tracing::instrument(skip_all, fields(job_id = %job.id, progress))]
    pub async fn set_job_progress(&self, job: &mut Job, progress: i64) -> AppResult<()> {
        job.status = job::JOB_STATUS_IN_PROGRESS.to_owned();
        job.progress = progress;

        let updated = self
            .store()
            .job()
            .update_optimistically(job, job::JOB_STATUS_IN_PROGRESS)
            .await
            .map_err(|err| job_update_error("SetJobProgress", &job.id, err))?;
        if let Some(updated) = updated {
            self.publish_job_status(&updated, job::JOB_STATUS_IN_PROGRESS)
                .await;
        }
        Ok(())
    }

    /// Port of `JobServer.SetJobSuccess` (jobs/jobs.go:167): an **unconditional** `UpdateStatus`,
    /// not the optimistic one. A job whose status has moved under it — to `cancel_requested`, say
    /// — is still driven to `success` here.
    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    pub async fn set_job_success(&self, job: &Job) -> AppResult<()> {
        self.set_job_status(&job.id, job::JOB_STATUS_SUCCESS, "SetJobSuccess")
            .await
    }

    /// Port of `JobServer.SetJobError` (jobs/jobs.go:181) — the longest of the transitions and
    /// the only one with two attempts.
    ///
    /// With **no** error (Go's `jobError == nil`) it is an unconditional `UpdateStatus(error)`
    /// and nothing else — no `Data`, no `Progress`.
    ///
    /// With one, three things happen to the job struct before the write:
    ///
    /// - `Status` becomes `error` and **`Progress` becomes `-1`**, which is not a status but is
    ///   how a client tells a failed job from one that stopped at 40%.
    /// - `Data["error"]` is built from the `AppError`: its message, then its `detailed_error`
    ///   after `" — "` when non-empty, then the wrapped error's text after another `" — "`. The
    ///   separator is U+2014 with a space on each side, verified against the Go bytes. Since
    ///   [`AppError::message`] is the id until something translates it ([D-092]), the first
    ///   segment on this server is `app.job.error` where Go's is the English sentence.
    /// - A `nil` `Data` map is created first, so the key always lands. The map is the **claimed
    ///   job's**, read back from the row, so whatever the job was created with survives beside
    ///   the new key — `UpdateOptimistically` replaces the column with this map, which is not the
    ///   same thing as replacing it with a fresh one.
    ///
    /// Then the write is attempted twice: optimistically against `in_progress`, and — if no row
    /// matched — against `cancel_requested`, which is the state a job is in when somebody asked
    /// to cancel it while it was running. Both missing is the 500 `jobs.set_job_error.update.error`,
    /// and it is the only transition that can fail without the store failing.
    ///
    /// The published status is **the row's**, not `error`: a job that failed after a cancellation
    /// request announces `cancel_requested`, because that is what was written… except that it is
    /// not — the write set `Status` to `error` from the struct. Go passes `ret.Status`, which by
    /// then is `error` in both branches. Reproduced as written rather than as it reads.
    #[tracing::instrument(skip_all, fields(job_id = %job.id, has_error = job_error.is_some()))]
    pub async fn set_job_error(
        &self,
        job: &mut Job,
        job_error: Option<&AppError>,
    ) -> AppResult<()> {
        let Some(job_error) = job_error else {
            return self
                .set_job_status(&job.id, job::JOB_STATUS_ERROR, "SetJobError")
                .await;
        };

        job.status = job::JOB_STATUS_ERROR.to_owned();
        job.progress = -1;
        let mut message = job_error.message.clone();
        if !job_error.detailed_error.is_empty() {
            message.push_str(" — ");
            message.push_str(&job_error.detailed_error);
        }
        if let Some(wrapped) = std::error::Error::source(job_error) {
            message.push_str(" — ");
            message.push_str(&wrapped.to_string());
        }
        job.data
            .get_or_insert_with(Default::default)
            .insert("error".to_owned(), message);

        let updated = self
            .store()
            .job()
            .update_optimistically(job, job::JOB_STATUS_IN_PROGRESS)
            .await
            .map_err(|err| job_update_error("SetJobError", &job.id, err))?;

        let updated = match updated {
            Some(updated) => updated,
            None => self
                .store()
                .job()
                .update_optimistically(job, job::JOB_STATUS_CANCEL_REQUESTED)
                .await
                .map_err(|err| job_update_error("SetJobError", &job.id, err))?
                .ok_or_else(|| {
                    AppError::boxed(
                        "SetJobError",
                        "jobs.set_job_error.update.error",
                        None,
                        format!("id={}", job.id),
                        500,
                    )
                })?,
        };

        let status = updated.status.clone();
        self.publish_job_status(&updated, &status).await;
        Ok(())
    }

    /// Port of `JobServer.CheckForPendingJobsByType` (jobs/jobs.go:363) — a `COUNT(*) > 0`.
    #[tracing::instrument(skip(self), fields(job_type = %job_type))]
    pub async fn check_for_pending_jobs_by_type(&self, job_type: &str) -> AppResult<bool> {
        let count = self
            .store()
            .job()
            .get_count_by_status_and_type(job::JOB_STATUS_PENDING, job_type)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, job_type, "pending job count failed");
                AppError::boxed(
                    "CheckForPendingJobsByType",
                    "app.job.get_count_by_status_and_type.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        Ok(count > 0)
    }

    /// Port of `JobServer.GetLastSuccessfulJobByType` (jobs/jobs.go:381).
    ///
    /// **`message_export` counts a `warning` run as successful and no other type does.** A
    /// message-export job that finished with warnings still moves the export cursor, so treating
    /// it as a failure would re-export the same window forever.
    ///
    /// `ErrNotFound` is `Ok(None)`, not an error: a type that has never run successfully is the
    /// ordinary case on a new deployment, and the scheduler is built to take `nil` there.
    #[tracing::instrument(skip(self), fields(job_type = %job_type, found))]
    pub async fn get_last_successful_job_by_type(&self, job_type: &str) -> AppResult<Option<Job>> {
        let statuses: Vec<String> = success_statuses_for(job_type)
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        match self
            .store()
            .job()
            .get_newest_job_by_statuses_and_type(&statuses, job_type)
            .await
        {
            Ok(job) => {
                tracing::Span::current().record("found", true);
                Ok(Some(job))
            }
            Err(StoreError::NotFound { .. }) => {
                tracing::Span::current().record("found", false);
                Ok(None)
            }
            Err(err) => {
                tracing::error!(error = %err, job_type, "last successful job lookup failed");
                Err(AppError::boxed(
                    "GetLastSuccessfulJobByType",
                    "app.job.get_newest_job_by_status_and_type.app_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }
}

/// The statuses `GetLastSuccessfulJobByType` counts as successful (jobs/jobs.go:382-385).
///
/// **`message_export` is the only type for which `warning` counts.** An export that finished with
/// warnings still moved its cursor, so calling it a failure would re-export the same window on
/// every run. Extracted from [`App::get_last_successful_job_by_type`] so that the special case
/// has a test that does not need a `message_export` row in the shared `Jobs` table — one of those
/// is visible to `GET /api/v4/jobs`, which the parity suite reads.
pub fn success_statuses_for(job_type: &str) -> &'static [&'static str] {
    if job_type == job::JOB_TYPE_MESSAGE_EXPORT {
        &[job::JOB_STATUS_WARNING, job::JOB_STATUS_SUCCESS]
    } else {
        &[job::JOB_STATUS_SUCCESS]
    }
}

/// The `app.job.update.app_error` every transition maps a store failure to, differing only in the
/// `Where`. Go writes the five arguments out at each site; one helper is the same value.
fn job_update_error(where_: &'static str, job_id: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, job_id, "job update failed");
    AppError::boxed(where_, "app.job.update.app_error", None, String::new(), 500)
}

// ---------------------------------------------------------------------------
// DoJob and the watcher
// ---------------------------------------------------------------------------

/// Port of `(*SimpleWorker).DoJob` (base_workers.go:69), including its two surprises.
///
/// **A lost claim is silent.** `ClaimJob` answering nil means another node took the job; Go
/// returns with no log and no state change, and so does this.
///
/// **`setJobSuccess` does not stop at its first failure.** Go's:
///
/// ```go
/// if err := worker.jobServer.SetJobProgress(job, 100); err != nil {
///     worker.setJobError(logger, job, err)
/// }
/// if err := worker.jobServer.SetJobSuccess(job); err != nil {
///     worker.setJobError(logger, job, err)
/// }
/// ```
///
/// There is no `return` between them, so a job whose progress write failed is marked `error` and
/// then immediately driven to `success` anyway. It reads like a bug and it is what runs;
/// reproduced, with a test on exactly that order.
///
/// # A panic is a failed job here and a dead server in Go
///
/// Each worker body `defer`s `HandleJobPanic`, which logs, calls `SetJobError` and then
/// **repanics** — out of `go w.Run()`, which has no recover above it, so the process dies. A
/// panicking task in tokio cannot take the process down, and making it do so would be inventing
/// behaviour rather than porting it. The job's own outcome — `status=error`, and
/// `HandleJobPanic`'s own `app.job.update.app_error` in `Data["error"]` — is identical, and that
/// is the part a client can see. A permanent, deliberate divergence, recorded here rather than in
/// the backlog because nothing is owed.
pub async fn do_job(app: App, slot: Arc<WorkerSlot>, job: Job) {
    let worker_name = slot.worker().name;
    let outcome = do_job_inner(&app, slot.worker(), job).await;
    slot.release();
    if let Err(err) = outcome {
        tracing::error!(error = %err, worker = worker_name, "worker: job did not complete");
    }
}

async fn do_job_inner(app: &App, worker: &SimpleWorker, job: Job) -> AppResult<()> {
    let Some(mut job) = app.claim_job(&job).await? else {
        // Somebody else claimed it. Go returns here with nothing logged.
        return Ok(());
    };

    let execute = (worker.execute)(app.clone(), job.clone());
    // Spawned rather than awaited in place so that a panicking job body is a `JoinError` here
    // instead of unwinding the watcher. See the doc comment.
    let result = tokio::spawn(execute).await;

    match result {
        Ok(Ok(())) => {
            tracing::debug!(job_id = %job.id, "SimpleWorker: Job is complete");
            set_job_success(app, &mut job).await;
            Ok(())
        }
        Ok(Err(err)) => {
            tracing::error!(error = %err, job_id = %job.id, "SimpleWorker: job execution error");
            let app_err = AppError::new("DoJob", "app.job.error", None, String::new(), 500)
                .wrap(ExecuteError(err));
            set_job_error(app, &mut job, &app_err).await;
            Ok(())
        }
        Err(join_err) => {
            // `HandleJobPanic`'s id, so the row reads the same as Go's.
            tracing::error!(error = %join_err, job_id = %job.id, "Unhandled panic in job");
            let app_err = AppError::new(
                "HandleJobPanic",
                "app.job.update.app_error",
                None,
                String::new(),
                500,
            );
            set_job_error(app, &mut job, &app_err).await;
            Ok(())
        }
    }
}

/// Port of `(*SimpleWorker).setJobSuccess` (base_workers.go:93). Both writes are attempted; see
/// [`do_job`].
async fn set_job_success(app: &App, job: &mut Job) {
    if let Err(err) = app.set_job_progress(job, 100).await {
        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to update progress for job");
        set_job_error(app, job, &err).await;
    }
    if let Err(err) = app.set_job_success(job).await {
        tracing::error!(error = %err, job_id = %job.id, "SimpleWorker: Failed to set success for job");
        set_job_error(app, job, &err).await;
    }
}

/// Port of `(*SimpleWorker).setJobError` (base_workers.go:105): a failure to record the failure
/// is logged and dropped.
async fn set_job_error(app: &App, job: &mut Job, app_error: &AppError) {
    if let Err(err) = app.set_job_error(job, Some(app_error)).await {
        tracing::error!(error = %err, job_id = %job.id, "SimpleWorker: Failed to set job error");
    }
}

/// Port of `jobs.Watcher` (jobs_watcher.go:19).
#[derive(Debug)]
pub struct Watcher {
    polling_interval_ms: u64,
}

impl Watcher {
    /// Port of `(*JobServer).MakeWatcher` (jobs_watcher.go:28).
    pub fn new(polling_interval_ms: u64) -> Self {
        Self {
            polling_interval_ms,
        }
    }

    pub fn polling_interval_ms(&self) -> u64 {
        self.polling_interval_ms
    }
}

impl App {
    /// Port of `(*Watcher).PollAndNotify` (jobs_watcher.go:70): read every `pending` job and
    /// offer each to the worker for its type.
    ///
    /// Returns the number of jobs that were *taken*, which is not the number read: a type with no
    /// registered worker is skipped, a disabled worker is skipped, and a busy one drops the offer
    /// (the module note explains why that is Go's unbuffered channel and not a bug).
    ///
    /// **Go does not consult `IsEnabled` here** — it does not have to, because a disabled worker
    /// has no goroutine parked on its channel and the `default:` arm fires. With no goroutines the
    /// check has to be explicit, and it is the same answer.
    ///
    /// A store failure is logged and swallowed, as Go's is: the watcher must survive to poll
    /// again.
    #[tracing::instrument(skip_all, fields(pending, dispatched))]
    pub async fn poll_and_notify(&self, workers: &Workers) -> usize {
        let jobs = match self
            .store()
            .job()
            .get_all_by_status(job::JOB_STATUS_PENDING)
            .await
        {
            Ok(jobs) => jobs,
            Err(err) => {
                tracing::error!(error = %err, "Error occurred getting all pending statuses.");
                return 0;
            }
        };
        tracing::Span::current().record("pending", jobs.len());

        let mut dispatched = 0;
        for job in jobs {
            let Some(slot) = workers.get(&job.job_type) else {
                continue;
            };
            if !slot.worker().is_enabled(&self.config()) {
                continue;
            }
            if !slot.take() {
                continue;
            }
            dispatched += 1;
            let app = self.clone();
            let slot = Arc::clone(slot);
            tokio::spawn(do_job(app, slot, job));
        }
        tracing::Span::current().record("dispatched", dispatched);
        dispatched
    }

    /// Port of `(*Watcher).Start` (jobs_watcher.go:38) together with `(*Workers).Start`
    /// (workers.go:45): poll for ever, at `polling_interval_ms`, after an initial random delay.
    ///
    /// The delay is Go's, and it is deliberate: "Delay for some random number of milliseconds
    /// before starting to ensure that multiple instances of the jobserver don't poll at a time
    /// too close to each other." It is uniform over `[0, interval)`.
    ///
    /// This never returns; the caller spawns it. Go's `Stop()` closes a channel that only the
    /// select reads — there is no shutdown path here because there is no caller that wants one.
    pub async fn run_watcher(self, workers: Arc<Workers>, watcher: Watcher) {
        let interval = watcher.polling_interval_ms();
        let delay = if interval == 0 {
            0
        } else {
            rand::Rng::random_range(&mut rand::rng(), 0..interval)
        };
        tracing::info!(
            interval_ms = interval,
            initial_delay_ms = delay,
            workers = workers.len(),
            "Starting workers"
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
            self.poll_and_notify(&workers).await;
        }
    }
}

// ---------------------------------------------------------------------------
// The registered workers
// ---------------------------------------------------------------------------

/// Port of `jobs/cleanup_desktop_tokens/worker.go`.
///
/// The whole body is one `DELETE`: every `DesktopTokens` row older than five minutes. Always
/// enabled — its `isEnabled` is `func(cfg *model.Config) bool { return true }`, with no setting
/// behind it.
///
/// **The cut-off is in Unix seconds**, because that is what the column holds; see
/// [`DesktopTokensStore::delete_older_than`].
pub fn cleanup_desktop_tokens_worker() -> SimpleWorker {
    /// `maxAge` (worker.go:15).
    const MAX_AGE_SECONDS: i64 = 5 * 60;

    SimpleWorker::new(
        "CleanupDesktopTokens",
        job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
        |_config| true,
        |app, _job| {
            Box::pin(async move {
                let cutoff = get_millis() / 1000 - MAX_AGE_SECONDS;
                app.store()
                    .desktop_tokens()
                    .delete_older_than(cutoff)
                    .await
                    .map_err(WorkerError::from)
            })
        },
    )
}

/// The workers this build registers, which is the Rust half of `Server.initJobs`
/// (app/server.go:1585).
///
/// One so far. [`crate::job::REGISTERED_JOB_TYPES`] lists the twenty-nine types the *Go* server
/// beside this one registers a worker for, and that list — not this one — is what
/// [`App::create_job`] validates against, because a job created here is run by whichever server
/// polls first. The two lists converge as workers are ported; until they do, a type in the first
/// and not the second is a job this server creates and the Go server runs.
pub fn registered_workers() -> Workers {
    let mut workers = Workers::new();
    workers.add(cleanup_desktop_tokens_worker());
    workers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_keyed_by_job_type_not_by_worker_name() {
        let workers = registered_workers();
        assert!(workers.get(job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS).is_some());
        assert!(
            workers.get("CleanupDesktopTokens").is_none(),
            "the worker's log name must not be a registry key"
        );
        assert_eq!(workers.len(), 1);
    }

    #[test]
    fn an_unregistered_type_is_gos_nil_worker() {
        let workers = registered_workers();
        assert!(workers.get(job::JOB_TYPE_PRODUCT_NOTICES).is_none());
        assert!(workers.get("").is_none());
    }

    /// `isEnabled` for this worker is the constant `true` — there is no setting behind it, so it
    /// runs on a default configuration and on every other one.
    #[test]
    fn cleanup_desktop_tokens_is_enabled_unconditionally() {
        let worker = cleanup_desktop_tokens_worker();
        assert!(worker.is_enabled(&Config::default()));
    }

    /// `message_export` is the one type that counts a `warning` run as successful, and the order
    /// of the two statuses is Go's.
    #[test]
    fn only_message_export_counts_a_warning_as_a_success() {
        assert_eq!(
            success_statuses_for(job::JOB_TYPE_MESSAGE_EXPORT),
            &["warning", "success"]
        );
        for job_type in [
            job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
            job::JOB_TYPE_LDAP_SYNC,
            job::JOB_TYPE_DATA_RETENTION,
            "",
        ] {
            assert_eq!(
                success_statuses_for(job_type),
                &["success"],
                "{job_type} must not count a warning"
            );
        }
    }

    /// The offer is Go's unbuffered send: it lands once and is dropped until the slot is
    /// released. A capacity-one channel would have taken the second offer too.
    #[test]
    fn a_busy_slot_drops_the_offer_rather_than_queueing_it() {
        let slot = WorkerSlot {
            worker: cleanup_desktop_tokens_worker(),
            busy: AtomicBool::new(false),
        };
        assert!(slot.take(), "an idle slot takes the offer");
        assert!(slot.is_busy());
        assert!(!slot.take(), "a busy slot drops it");
        assert!(!slot.take(), "and keeps dropping it");
        slot.release();
        assert!(slot.take(), "a released slot takes the next one");
    }
}
