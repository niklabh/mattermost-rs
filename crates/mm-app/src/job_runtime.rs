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
use mm_store::{
    DesktopTokensStore, DraftStore, JobStore, PreferenceStore, StoreError, SystemStore,
};

use crate::App;
use crate::config::Config;

/// Go's `jobs.DefaultWatcherPollingInterval` (jobs_watcher.go:16), in milliseconds.
///
/// A `var` rather than a `const` upstream, with the comment "Defining as `var` rather than
/// `const` allows tests to lower the interval" — [`Watcher::new`] takes it as an argument for the
/// same reason.
pub const DEFAULT_WATCHER_POLLING_INTERVAL_MS: u64 = 15_000;

/// Go's `jobs.CancelWatcherPollingInterval` (jobs.go:22), in milliseconds.
pub const CANCEL_WATCHER_POLLING_INTERVAL_MS: u64 = 5_000;

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
/// is inside `DoJob`. Generic over the two worker shapes, [`SimpleWorker`] and [`BatchWorker`],
/// whose offer semantics are the same unbuffered channel.
#[derive(Debug)]
pub struct Slot<W> {
    worker: W,
    /// `false` is Go's "parked on `<-worker.jobs`", the only state in which an offer lands.
    busy: AtomicBool,
}

/// The slot of a [`SimpleWorker`].
pub type WorkerSlot = Slot<SimpleWorker>;

/// The slot of a [`BatchWorker`].
pub type BatchSlot = Slot<BatchWorker>;

impl<W> Slot<W> {
    fn new(worker: W) -> Self {
        Self {
            worker,
            busy: AtomicBool::new(false),
        }
    }

    pub fn worker(&self) -> &W {
        &self.worker
    }

    /// Port of the watcher's `select { case ch <- job: default: }`: `true` when the offer was
    /// taken, `false` when it was dropped because the worker was mid-job.
    ///
    /// Taking the offer marks the slot busy; [`Slot::release`] is the return from `DoJob`.
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
///
/// The batch workers sit in a second map, so that [`Workers::add`] and [`Workers::get`] keep the
/// [`SimpleWorker`] shape they have always had; a job type is registered in one map or the other,
/// as each Go worker is one kind or the other.
#[derive(Debug, Default)]
pub struct Workers {
    workers: HashMap<&'static str, Arc<WorkerSlot>>,
    batch: HashMap<&'static str, Arc<BatchSlot>>,
}

impl Workers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Port of `(*Workers).AddWorker` (workers.go:36). Keyed by job type, as Go's callers key it:
    /// `RegisterJobType(model.JobTypeX, worker, scheduler)`.
    pub fn add(&mut self, worker: SimpleWorker) {
        self.workers
            .insert(worker.job_type, Arc::new(Slot::new(worker)));
    }

    /// [`Workers::add`] for a [`BatchWorker`].
    pub fn add_batch(&mut self, worker: BatchWorker) {
        self.batch
            .insert(worker.job_type, Arc::new(Slot::new(worker)));
    }

    /// [`Workers::get`] for a [`BatchWorker`].
    pub fn get_batch(&self, job_type: &str) -> Option<&Arc<BatchSlot>> {
        self.batch.get(job_type)
    }

    /// Port of `(*Workers).Get` (workers.go:40). A map miss is Go's nil interface, which is what
    /// `_createJob` turns into `model.job.is_valid.type.app_error` and what the watcher skips on.
    pub fn get(&self, job_type: &str) -> Option<&Arc<WorkerSlot>> {
        self.workers.get(job_type)
    }

    /// Every registered worker, of both shapes.
    pub fn len(&self) -> usize {
        self.workers.len() + self.batch.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty() && self.batch.is_empty()
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

    /// Port of `JobServer.UpdateInProgressJobData` (jobs/jobs.go:264): write the job's `Data`
    /// (and its `Progress`, which rides along in the same statement) while it is still
    /// `in_progress`, stamping `LastActivityAt`.
    ///
    /// Like [`App::set_job_progress`] it mutates the caller's job to `in_progress` first, and a job
    /// whose status has moved matches no row — which is **not** an error: the store's nil job is
    /// discarded and no event is published, because Go publishes none here at all.
    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    pub async fn update_in_progress_job_data(&self, job: &mut Job) -> AppResult<()> {
        job.status = job::JOB_STATUS_IN_PROGRESS.to_owned();
        job.last_activity_at = get_millis();
        self.store()
            .job()
            .update_optimistically(job, job::JOB_STATUS_IN_PROGRESS)
            .await
            .map_err(|err| job_update_error("UpdateInProgressJobData", &job.id, err))?;
        Ok(())
    }

    /// Port of `JobServer.CancellationWatcher` (jobs/jobs.go:328): poll the job every
    /// [`CANCEL_WATCHER_POLLING_INTERVAL_MS`] and **return** once it reads `cancel_requested` —
    /// Go's `close(cancelChan)`.
    ///
    /// Go's other exit, the context being done because the job finished, is the caller dropping
    /// this future. A failed read is logged and the next poll tried, as Go's `continue` does; a
    /// job that has gone to any other status is simply polled again.
    ///
    /// Go's workers that race this against their batches are the `migrations` worker and — for
    /// `UpdateInProgressJobData` — `extract_content`; neither is ported yet, and the three batch
    /// workers here do **not** use it (see [`BatchWorker`]). `interval_ms` is a parameter for the
    /// reason [`Watcher::new`]'s is.
    #[tracing::instrument(skip(self), fields(job_id = %job_id))]
    pub async fn cancellation_watcher(&self, job_id: &str, interval_ms: u64) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            tracing::debug!(job_id, "CancellationWatcher for Job started polling.");
            match self.store().job().get(job_id).await {
                Ok(job) if job.status == job::JOB_STATUS_CANCEL_REQUESTED => return,
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, job_id, "Error getting job"),
            }
        }
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
            if let Some(slot) = workers.get(&job.job_type) {
                if !slot.worker().is_enabled(&self.config()) {
                    continue;
                }
                if !slot.take() {
                    continue;
                }
                dispatched += 1;
                tokio::spawn(do_job(self.clone(), Arc::clone(slot), job));
            } else if let Some(slot) = workers.get_batch(&job.job_type) {
                // `BatchWorker.IsEnabled` is the constant `true`.
                if !slot.take() {
                    continue;
                }
                dispatched += 1;
                tokio::spawn(do_batch_job(self.clone(), Arc::clone(slot), job));
            }
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
// The batch worker shape
// ---------------------------------------------------------------------------

/// The future one batch returns: the job as the batch left it, and whether the worker stops
/// (Go's `doBatch` returning `true`).
pub type BatchFuture = Pin<Box<dyn Future<Output = (Job, bool)> + Send>>;

/// Port of `jobs.BatchWorker` (batch_worker.go:21) — a worker that calls `doBatch` every
/// `timeBetweenBatches` until it answers "stop".
///
/// # What a batch does to the row, and what a cancellation does to a batch
///
/// Between batches the only write is the one `doBatch` makes, and for both kinds built on this
/// ([`batch_migration_worker`], [`batch_report_worker`]) that is `SetJobProgress(job, 0)` — the
/// cursor in `Data`, progress held at 0. **There is no cancellation watcher on this shape.** A job
/// moved to `cancel_requested` while it runs keeps running: every `SetJobProgress` then matches no
/// row and is dropped silently, and when the batches run out, `SetJobProgress(100)` is dropped
/// the same way while `SetJobSuccess` — unconditional — writes `success`. So a cancelled batch job
/// ends `success` with the progress and data of its last write before the request. Go's; nothing
/// in the public tree cancels one of these from outside either — `SessionHasPermissionToCreateJob`
/// names none of the four types, so the API refuses to cancel them.
///
/// # Stop
///
/// Go's `Stop` closes a channel `DoJob` selects on and puts the job back to `pending`. This
/// server has no worker shutdown path (see [`App::run_watcher`]), so that arm is absent; a
/// process that exits mid-job leaves it `in_progress`, as a killed Go process does.
pub struct BatchWorker {
    /// `worker_name` in Go's log fields.
    pub name: &'static str,
    /// The `Jobs.Type` this worker claims.
    pub job_type: &'static str,
    time_between_batches: std::time::Duration,
    do_batch: Arc<dyn Fn(App, Job) -> BatchFuture + Send + Sync>,
}

impl std::fmt::Debug for BatchWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchWorker")
            .field("name", &self.name)
            .field("job_type", &self.job_type)
            .field("time_between_batches", &self.time_between_batches)
            .finish_non_exhaustive()
    }
}

impl BatchWorker {
    /// Port of `jobs.MakeBatchWorker` (batch_worker.go:34).
    pub fn new(
        name: &'static str,
        job_type: &'static str,
        time_between_batches: std::time::Duration,
        do_batch: impl Fn(App, Job) -> BatchFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            job_type,
            time_between_batches,
            do_batch: Arc::new(do_batch),
        }
    }

    /// Port of `(*BatchWorker).IsEnabled` (batch_worker.go:111) — always `true`.
    pub fn is_enabled(&self, _config: &Config) -> bool {
        true
    }

    pub fn time_between_batches(&self) -> std::time::Duration {
        self.time_between_batches
    }

    /// The same worker registered under another job type.
    ///
    /// Nothing in the server calls this. It exists so that a test can let **this** server's
    /// worker run a job end to end — claim, batches, the terminal writes — on a type no Go server
    /// registers, since every Go process on a stack polls the shared `Jobs` table and would
    /// otherwise race for the claim. No worker body reads the type.
    pub fn with_job_type(mut self, job_type: &'static str) -> Self {
        self.job_type = job_type;
        self
    }
}

/// Port of `(*BatchWorker).DoJob` (batch_worker.go:116): claim, give a nil `Data` an empty map,
/// then one batch every `time_between_batches` — the wait comes **first** — until a batch says
/// stop.
///
/// A panicking batch lands in `error` as [`do_job`]'s body does; the job written is the one the
/// claim returned, not the last batch's, because the batch that panicked owned it.
pub async fn do_batch_job(app: App, slot: Arc<BatchSlot>, job: Job) {
    let worker_name = slot.worker().name;
    let outcome = do_batch_job_inner(&app, slot.worker(), job).await;
    slot.release();
    if let Err(err) = outcome {
        tracing::warn!(error = %err, worker = worker_name, "Worker experienced an error while trying to claim job");
    }
}

async fn do_batch_job_inner(app: &App, worker: &BatchWorker, job: Job) -> AppResult<()> {
    let Some(mut job) = app.claim_job(&job).await? else {
        return Ok(());
    };
    if job.data.is_none() {
        job.data = Some(mm_model::utils::StringMap::new());
    }

    let claimed = job.clone();
    let run = run_batches(
        app.clone(),
        Arc::clone(&worker.do_batch),
        worker.time_between_batches,
        job,
    );
    if let Err(join_err) = tokio::spawn(run).await {
        tracing::error!(error = %join_err, job_id = %claimed.id, "Unhandled panic in job");
        let mut claimed = claimed;
        let app_err = AppError::new(
            "HandleJobPanic",
            "app.job.update.app_error",
            None,
            String::new(),
            500,
        );
        set_job_error(app, &mut claimed, &app_err).await;
    }
    Ok(())
}

async fn run_batches(
    app: App,
    do_batch: Arc<dyn Fn(App, Job) -> BatchFuture + Send + Sync>,
    time_between_batches: std::time::Duration,
    mut job: Job,
) {
    loop {
        tokio::time::sleep(time_between_batches).await;
        let (next, stop) = do_batch(app.clone(), job).await;
        job = next;
        if stop {
            return;
        }
    }
}

/// `model.NoTranslation`, the id both batch shapes wrap their failures in.
const NO_TRANSLATION: &str = mm_model::utils::NO_TRANSLATION;

/// The failure one migration or report batch returns — Go's plain `error`.
pub type BatchError = WorkerError;

/// The future of one migration batch: the next `Data` (Go's `nextData`, which may be nil), and
/// whether the migration is done.
pub type MigrationBatchFuture = Pin<
    Box<dyn Future<Output = Result<(Option<mm_model::utils::StringMap>, bool), BatchError>> + Send>,
>;

/// Port of `jobs.MakeBatchMigrationWorker` (batch_migration_worker.go:38) and its `doBatch`.
///
/// Per batch, in Go's order:
///
/// 1. `checkIsClusterInSync` — `GetClusterStatus` with a nil cluster interface, which every build
///    from this tree has, is an **empty** list, and an empty list is in sync. So `resetJob`, the
///    out-of-sync arm, is unreachable here and not ported.
/// 2. `doMigrationBatch(job.Data)`. An error is `SetJobError` with `model.NoTranslation`
///    wrapping it, and stop.
/// 3. Done: [`set_job_success`] (progress 100, then `success`), then `markAsComplete` — a plain
///    `System.Save` of `migration_key = "true"`, whose failure (the row already there) is only
///    logged — and stop.
/// 4. Otherwise `Data` becomes the next cursor and `SetJobProgress(job, 0)` writes it; a failed
///    write is logged and the next batch runs anyway.
pub fn batch_migration_worker(
    name: &'static str,
    job_type: &'static str,
    migration_key: &'static str,
    time_between_batches: std::time::Duration,
    do_migration_batch: impl Fn(App, Option<mm_model::utils::StringMap>) -> MigrationBatchFuture
    + Send
    + Sync
    + 'static,
) -> BatchWorker {
    let do_migration_batch = Arc::new(do_migration_batch);
    BatchWorker::new(name, job_type, time_between_batches, move |app, mut job| {
        let do_migration_batch = Arc::clone(&do_migration_batch);
        Box::pin(async move {
            match do_migration_batch(app.clone(), job.data.clone()).await {
                Err(err) => {
                    tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to do migration batch. Exiting");
                    let app_err =
                        AppError::new("doMigrationBatch", NO_TRANSLATION, None, String::new(), 500)
                            .wrap(ExecuteError(err));
                    set_job_error(&app, &mut job, &app_err).await;
                    (job, true)
                }
                Ok((_, true)) => {
                    tracing::info!(job_id = %job.id, "Worker: Job is complete");
                    set_job_success(&app, &mut job).await;
                    if let Err(err) = app.store().system().save(migration_key, "true").await {
                        tracing::error!(error = %err, migration_key, "Worker: Failed to mark migration as completed in the systems table.");
                    }
                    (job, true)
                }
                Ok((next, false)) => {
                    job.data = next;
                    if let Err(err) = app.set_job_progress(&mut job, 0).await {
                        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to set job progress");
                    }
                    (job, false)
                }
            }
        })
    })
}

/// The future of one report batch: `None` when there is nothing left (Go's `done`), otherwise
/// the chunk's rows, each already `ToReport()`ed.
pub type ReportBatchFuture = Pin<
    Box<
        dyn Future<
                Output = Result<(Option<Vec<Vec<String>>>, mm_model::utils::StringMap), BatchError>,
            > + Send,
    >,
>;

/// Port of `jobs.MakeBatchReportWorker` (batch_report_worker.go:35) and its `doBatch`.
///
/// `get_data` is handed the job's `Data` and gives back the map to continue with. Go's
/// `getData` returns a `nextData` that — for its one user, `export_users_to_csv` — **is the same
/// map** it was given, mutated; `processChunk` then writes `file_count` into `job.Data` before
/// `job.Data = nextData`, and the aliasing is why the count survives. Here the chunk's
/// `file_count` is written into the map `get_data` returned, which is that same result.
///
/// Per batch: `get_data` (an error is `SetJobError` with `NoTranslation`, and stop); nothing left
/// is `complete` — compile the chunks under the headers, then the report to the requester, the
/// chunks removed afterwards whatever that answered — and then success or the error; otherwise
/// the chunk is saved as number `file_count` (0 when absent), `file_count` goes up by one, and
/// `SetJobProgress(job, 0)` writes the map.
pub fn batch_report_worker(
    name: &'static str,
    job_type: &'static str,
    time_between_batches: std::time::Duration,
    report_format: &'static str,
    headers: &'static [&'static str],
    get_data: impl Fn(App, mm_model::utils::StringMap) -> ReportBatchFuture + Send + Sync + 'static,
) -> BatchWorker {
    let get_data = Arc::new(get_data);
    BatchWorker::new(name, job_type, time_between_batches, move |app, mut job| {
        let get_data = Arc::clone(&get_data);
        Box::pin(async move {
            let data = job.data.clone().unwrap_or_default();
            let fail = |err: BatchError| {
                AppError::new("doBatch", NO_TRANSLATION, None, String::new(), 500)
                    .wrap(ExecuteError(err))
            };
            match get_data(app.clone(), data).await {
                Err(err) => {
                    tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to get data for report batch. Exiting");
                    set_job_error(&app, &mut job, &fail(err)).await;
                }
                Ok((None, _)) => match complete_report(&app, &job, report_format, headers).await {
                    Err(err) => {
                        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to finish the batch report. Exiting");
                        set_job_error(&app, &mut job, &fail(err)).await;
                    }
                    Ok(()) => {
                        tracing::info!(job_id = %job.id, "Worker: Report job complete");
                        set_job_success(&app, &mut job).await;
                    }
                },
                Ok((Some(rows), mut next)) => {
                    if let Err(err) =
                        process_report_chunk(&app, &job.id, &mut next, report_format, &rows).await
                    {
                        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to save report batch. Exiting");
                        set_job_error(&app, &mut job, &fail(err)).await;
                        return (job, true);
                    }
                    job.data = Some(next);
                    if let Err(err) = app.set_job_progress(&mut job, 0).await {
                        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to set job progress");
                    }
                    return (job, false);
                }
            }
            (job, true)
        })
    })
}

/// Port of `jobs.getFileCount` (batch_report_worker.go:96): `strconv.Atoi` of `file_count`, or
/// 0 when the key is absent or empty.
fn report_file_count(data: &mm_model::utils::StringMap) -> Result<i64, BatchError> {
    match data.get("file_count").map(String::as_str) {
        None | Some("") => Ok(0),
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|err| BatchError::from(format!("failed to parse file_count: {err}"))),
    }
}

/// Port of `(*BatchReportWorker).processChunk` (batch_report_worker.go:109).
async fn process_report_chunk(
    app: &App,
    job_id: &str,
    data: &mut mm_model::utils::StringMap,
    report_format: &str,
    rows: &[Vec<String>],
) -> Result<(), BatchError> {
    let file_count = report_file_count(data)?;
    app.save_report_chunk(report_format, job_id, file_count, rows)
        .await
        .map_err(|err| BatchError::from(*err))?;
    data.insert("file_count".to_owned(), (file_count + 1).to_string());
    Ok(())
}

/// Port of `(*BatchReportWorker).complete` (batch_report_worker.go:124): compile, then send, and
/// the chunks cleaned up once the compile has succeeded — Go's `defer` — whatever the send did.
async fn complete_report(
    app: &App,
    job: &Job,
    report_format: &str,
    headers: &[&str],
) -> Result<(), BatchError> {
    let file_count = report_file_count(job.data.as_ref().unwrap_or(&Default::default()))?;
    app.compile_report_chunks(report_format, &job.id, file_count, headers)
        .await
        .map_err(|err| BatchError::from(*err))?;
    let sent = app.send_report_to_user(job, report_format).await;
    if let Err(err) = app
        .cleanup_report_chunks(report_format, &job.id, file_count)
        .await
    {
        tracing::error!(error = %err, job_id = %job.id, "Worker: Failed to cleanup report chunks");
    }
    sent.map_err(|err| BatchError::from(*err))
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

/// Port of `jobs/post_persistent_notifications/worker.go`: enabled while
/// `IsPersistentNotificationsEnabled` — `ServiceSettings.PostPriority` and
/// `AllowPersistentNotifications` — and the body is [`App::send_persistent_notifications`].
pub fn post_persistent_notifications_worker() -> SimpleWorker {
    SimpleWorker::new(
        "PostPersistentNotifications",
        job::JOB_TYPE_POST_PERSISTENT_NOTIFICATIONS,
        |config| config.post_priority && config.allow_persistent_notifications,
        |app, _job| {
            Box::pin(async move {
                app.send_persistent_notifications()
                    .await
                    .map_err(WorkerError::from)
            })
        },
    )
}

/// Port of `jobs/product_notices/worker.go`: enabled while either notice switch
/// (`AnnouncementSettings.AdminNoticesEnabled` or `UserNoticesEnabled`) is on, and the body is
/// [`App::update_product_notices`] — logged and returned when it fails. It refreshes *this
/// process's* notice cache, as Go's refreshes the cache of the node that claimed the job.
pub fn product_notices_worker() -> SimpleWorker {
    SimpleWorker::new(
        "ProductNotices",
        job::JOB_TYPE_PRODUCT_NOTICES,
        |config| config.admin_notices_enabled || config.user_notices_enabled,
        |app, _job| {
            Box::pin(async move {
                app.update_product_notices().await.map_err(|err| {
                    tracing::error!(error = %err, "Worker: Failed to fetch product notices");
                    WorkerError::from(err)
                })
            })
        },
    )
}

/// Port of `notify_admin.MakeInstallPluginNotifyWorker` (jobs/notify_admin/worker.go:60): always
/// enabled, and the body is [`App::do_check_for_admin_notifications`] with `trial` false.
///
/// Its two siblings — `upgrade_notify_admin` and `trial_notify_admin` — are **not** registered:
/// their `isEnabled` is the licence captured at start-up having `Features.Cloud`, and a
/// [`SimpleWorker`]'s `is_enabled` sees only [`Config`]. [D-1321].
pub fn install_plugin_notify_admin_worker() -> SimpleWorker {
    SimpleWorker::new(
        "InstallNotifyAdmin",
        job::JOB_TYPE_INSTALL_PLUGIN_NOTIFY_ADMIN,
        |_config| true,
        |app, _job| {
            Box::pin(async move {
                app.do_check_for_admin_notifications(false)
                    .await
                    .map_err(WorkerError::from)
            })
        },
    )
}

/// `timeBetweenBatches` of all four batch workers (1 second each).
const ONE_SECOND: std::time::Duration = std::time::Duration::from_secs(1);

/// Which of the two draft migrations a batch deletes for.
#[derive(Debug, Clone, Copy)]
enum DraftSweep {
    Empty,
    Orphan,
}

/// `parseJobMetadata` of the two draft migrations: `create_at` (absent or empty is 0, anything
/// else `strconv.ParseInt`) and `user_id` as it is.
fn draft_cursor(data: &mm_model::utils::StringMap) -> Result<(i64, String), BatchError> {
    let create_at = match data.get("create_at").map(String::as_str) {
        None | Some("") => 0,
        Some(raw) => raw.parse::<i64>().map_err(|err| {
            BatchError::from(format!(
                "failed to parse job metadata: failed to parse create_at: {err}"
            ))
        })?,
    };
    Ok((create_at, data.get("user_id").cloned().unwrap_or_default()))
}

/// `doDeleteEmptyDraftsMigrationBatch` / `doDeleteOrphanDraftsMigrationBatch`
/// (jobs/delete_*_drafts_migration): read the next window's last `(CreateAt, UserId)`; none is
/// done; otherwise delete this window's matching drafts and answer that pair as the next cursor.
///
/// **The delete is keyed on the cursor as it was**, not the new one: the window read and the
/// window deleted are the same 100 rows after the old cursor.
async fn draft_migration_batch(
    app: App,
    data: Option<mm_model::utils::StringMap>,
    sweep: DraftSweep,
) -> Result<(Option<mm_model::utils::StringMap>, bool), BatchError> {
    let (create_at, user_id) = draft_cursor(&data.unwrap_or_default())?;
    let drafts = app.store().draft();
    let (next_create_at, next_user_id) = drafts
        .get_last_create_at_and_user_id_values_for_empty_drafts_migration(create_at, &user_id)
        .await
        .map_err(|err| {
            BatchError::from(format!(
                "failed to get the next batch (create_at={create_at}, user_id={user_id}): {err}"
            ))
        })?;
    if next_create_at == 0 && next_user_id.is_empty() {
        return Ok((None, true));
    }
    let deleted = match sweep {
        DraftSweep::Empty => {
            drafts
                .delete_empty_drafts_by_create_at_and_user_id(create_at, &user_id)
                .await
        }
        DraftSweep::Orphan => {
            drafts
                .delete_orphan_drafts_by_create_at_and_user_id(create_at, &user_id)
                .await
        }
    };
    deleted.map_err(|err| {
        let what = match sweep {
            DraftSweep::Empty => "empty",
            DraftSweep::Orphan => "orphan",
        };
        BatchError::from(format!(
            "failed to delete {what} drafts (create_at={create_at}, user_id={user_id}): {err}"
        ))
    })?;
    let mut next = mm_model::utils::StringMap::new();
    next.insert("create_at".to_owned(), next_create_at.to_string());
    next.insert("user_id".to_owned(), next_user_id);
    Ok((Some(next), false))
}

/// Port of `jobs/delete_empty_drafts_migration`: every draft whose message is empty, 100 rows of
/// the `(CreateAt, UserId)` order at a time, one batch a second.
pub fn delete_empty_drafts_migration_worker() -> BatchWorker {
    batch_migration_worker(
        "DeleteEmptyDraftsMigration",
        job::JOB_TYPE_DELETE_EMPTY_DRAFTS_MIGRATION,
        mm_model::migration::MIGRATION_KEY_DELETE_EMPTY_DRAFTS,
        ONE_SECOND,
        |app, data| Box::pin(draft_migration_batch(app, data, DraftSweep::Empty)),
    )
}

/// Port of `jobs/delete_orphan_drafts_migration`: every draft whose root post is deleted or
/// missing — which, for a draft with no root, is every channel draft; see
/// [`DraftStore::delete_orphan_drafts_by_create_at_and_user_id`].
pub fn delete_orphan_drafts_migration_worker() -> BatchWorker {
    batch_migration_worker(
        "DeleteOrphanDraftsMigration",
        job::JOB_TYPE_DELETE_ORPHAN_DRAFTS_MIGRATION,
        mm_model::migration::MIGRATION_KEY_DELETE_ORPHAN_DRAFTS,
        ONE_SECOND,
        |app, data| Box::pin(draft_migration_batch(app, data, DraftSweep::Orphan)),
    )
}

/// Port of `jobs/delete_dms_preferences_migration`: delete up to 100 out-of-range
/// `limit_visible_dms_gms` preferences a batch until a batch deletes none. The next data is Go's
/// **nil** map every time, so the row's `Data` is `null` between batches.
pub fn delete_dms_preferences_migration_worker() -> BatchWorker {
    batch_migration_worker(
        "DeleteDmsPreferencesMigration",
        job::JOB_TYPE_DELETE_DMS_PREFERENCES_MIGRATION,
        mm_model::migration::MIGRATION_KEY_DELETE_DMS_PREFERENCES,
        ONE_SECOND,
        |app, _data| {
            Box::pin(async move {
                let deleted = app
                    .store()
                    .preference()
                    .delete_invalid_visible_dms_gms()
                    .await
                    .map_err(|err| {
                        BatchError::from(format!(
                            "failed to delete invalid limit_visible_dms_gms: {err}"
                        ))
                    })?;
                Ok((None, deleted == 0))
            })
        },
    )
}

/// `csvExportColumns` (export_users_to_csv.go:18), the header row of the user export.
pub const CSV_EXPORT_COLUMNS: [&str; 14] = [
    "Id",
    "Username",
    "Email",
    "CreateAt",
    "Name",
    "Roles",
    "LastLogin",
    "LastStatusAt",
    "LastPostDate",
    "DaysActive",
    "TotalPosts",
    "ChannelCount",
    "Teams",
    "DeletedAt",
];

/// `parseJobMetadata` of `export_users_to_csv` (export_users_to_csv.go:61): the report options
/// the job's data describes.
///
/// `start_at` and `end_at` are **required** — `strconv.ParseInt("")` is an error, so a job
/// created without them fails on its first batch. The two `hide_*` flags are optional and read
/// with `strconv.ParseBool`'s six spellings each way. Sort by `Username`, 100 a page, the cursor
/// from `last_column_value` and `last_user_id`.
fn export_users_options(
    data: &mm_model::utils::StringMap,
) -> Result<mm_model::report::UserReportOptions, BatchError> {
    let get = |key: &str| data.get(key).map(String::as_str).unwrap_or("");
    let int = |key: &str| {
        get(key).parse::<i64>().map_err(|err| {
            BatchError::from(format!(
                "failed to parse job metadata: strconv.ParseInt: parsing {:?}: {err}",
                get(key)
            ))
        })
    };
    let start_at = int("start_at")?;
    let end_at = int("end_at")?;
    let flag = |key: &str| -> Result<bool, BatchError> {
        match get(key) {
            "" => Ok(false),
            raw => crate::config::parse_bool(raw).ok_or_else(|| {
                BatchError::from(format!(
                    "failed to parse job metadata: failed to parse {key}: strconv.ParseBool: parsing {raw:?}: invalid syntax"
                ))
            }),
        }
    };
    Ok(mm_model::report::UserReportOptions {
        base: mm_model::report::ReportingBaseOptions {
            sort_column: "Username".to_owned(),
            page_size: 100,
            from_column_value: get("last_column_value").to_owned(),
            from_id: get("last_user_id").to_owned(),
            start_at,
            end_at,
            ..Default::default()
        },
        hide_inactive: flag("hide_inactive")?,
        hide_active: flag("hide_active")?,
        role: get("role").to_owned(),
        team: get("team").to_owned(),
        guest_filter: get("guest_filter").to_owned(),
        ..Default::default()
    })
}

/// Port of `jobs/export_users_to_csv`: the System Console's user export, 100 users a chunk in
/// username order, compiled into one CSV and posted to the requester by the system bot. The
/// cursor — the last row's username and id — is written into the job's own data map, which is
/// what [`batch_report_worker`] relies on.
pub fn export_users_to_csv_worker() -> BatchWorker {
    batch_report_worker(
        "ExportUsersToCSV",
        job::JOB_TYPE_EXPORT_USERS_TO_CSV,
        ONE_SECOND,
        "csv",
        &CSV_EXPORT_COLUMNS,
        |app, mut data| {
            Box::pin(async move {
                let options = export_users_options(&data)?;
                let users = app.get_users_for_reporting(&options).await.map_err(|err| {
                    BatchError::from(format!(
                        "failed to get the next batch (column_value={}, user_id={}): {err}",
                        options.base.from_column_value, options.base.from_id
                    ))
                })?;
                let Some(last) = users.last() else {
                    return Ok((None, data));
                };
                data.insert("last_column_value".to_owned(), last.user.username.clone());
                data.insert("last_user_id".to_owned(), last.user.id.clone());
                let rows = users.iter().map(|user| user.to_report()).collect();
                Ok((Some(rows), data))
            })
        },
    )
}

/// The workers this build registers, which is the Rust half of `Server.initJobs`
/// (app/server.go:1585).
///
/// A few so far. [`crate::job::REGISTERED_JOB_TYPES`] lists the twenty-nine types the *Go* server
/// beside this one registers a worker for, and that list — not this one — is what
/// [`App::create_job`] validates against, because a job created here is run by whichever server
/// polls first. The two lists converge as workers are ported; until they do, a type in the first
/// and not the second is a job this server creates and the Go server runs.
pub fn registered_workers() -> Workers {
    let mut workers = Workers::new();
    workers.add(cleanup_desktop_tokens_worker());
    workers.add(post_persistent_notifications_worker());
    workers.add(product_notices_worker());
    workers.add(install_plugin_notify_admin_worker());
    workers.add_batch(delete_empty_drafts_migration_worker());
    workers.add_batch(delete_orphan_drafts_migration_worker());
    workers.add_batch(export_users_to_csv_worker());
    workers.add_batch(delete_dms_preferences_migration_worker());
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
        assert!(
            workers
                .get(job::JOB_TYPE_POST_PERSISTENT_NOTIFICATIONS)
                .is_some()
        );
        assert!(workers.get(job::JOB_TYPE_PRODUCT_NOTICES).is_some());
        assert!(
            workers
                .get(job::JOB_TYPE_INSTALL_PLUGIN_NOTIFY_ADMIN)
                .is_some()
        );
        assert_eq!(workers.len(), 8);
        for job_type in [
            job::JOB_TYPE_DELETE_EMPTY_DRAFTS_MIGRATION,
            job::JOB_TYPE_DELETE_ORPHAN_DRAFTS_MIGRATION,
            job::JOB_TYPE_EXPORT_USERS_TO_CSV,
            job::JOB_TYPE_DELETE_DMS_PREFERENCES_MIGRATION,
        ] {
            assert!(workers.get_batch(job_type).is_some(), "{job_type}");
            assert!(
                workers.get(job_type).is_none(),
                "{job_type} is not a SimpleWorker"
            );
        }
    }

    #[test]
    fn an_unregistered_type_is_gos_nil_worker() {
        let workers = registered_workers();
        assert!(workers.get(job::JOB_TYPE_LDAP_SYNC).is_none());
        assert!(workers.get("").is_none());
    }

    /// `isEnabled` for this worker is the constant `true` — there is no setting behind it, so it
    /// runs on a default configuration and on every other one.
    #[test]
    fn cleanup_desktop_tokens_is_enabled_unconditionally() {
        let worker = cleanup_desktop_tokens_worker();
        assert!(worker.is_enabled(&Config::default()));
    }

    fn map(pairs: &[(&str, &str)]) -> mm_model::utils::StringMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The draft cursor: absent or empty `create_at` is 0, anything else `ParseInt` — a sign is
    /// accepted, a fraction is the batch's error — and `user_id` is taken as it is.
    #[test]
    fn the_draft_cursor_parses_as_go_does() {
        assert_eq!(draft_cursor(&map(&[])).unwrap(), (0, String::new()));
        assert_eq!(
            draft_cursor(&map(&[("create_at", ""), ("user_id", "u")])).unwrap(),
            (0, "u".to_owned())
        );
        assert_eq!(
            draft_cursor(&map(&[("create_at", "+42")])).unwrap(),
            (42, String::new())
        );
        assert_eq!(draft_cursor(&map(&[("create_at", "-1")])).unwrap().0, -1);
        assert!(draft_cursor(&map(&[("create_at", "1.5")])).is_err());
        assert!(draft_cursor(&map(&[("create_at", " 1")])).is_err());
    }

    /// `file_count`: absent and empty are 0; anything else must be an integer.
    #[test]
    fn the_report_file_count_is_atoi_or_zero() {
        assert_eq!(report_file_count(&map(&[])).unwrap(), 0);
        assert_eq!(report_file_count(&map(&[("file_count", "")])).unwrap(), 0);
        assert_eq!(report_file_count(&map(&[("file_count", "3")])).unwrap(), 3);
        assert!(report_file_count(&map(&[("file_count", "x")])).is_err());
    }

    /// The export's options: `start_at` and `end_at` are required; the `hide_*` flags take
    /// `ParseBool`'s spellings; the cursor and filters are copied; sort by username, 100 a page.
    #[test]
    fn the_export_options_follow_parse_job_metadata() {
        let data = map(&[
            ("start_at", "5"),
            ("end_at", "9"),
            ("hide_active", "T"),
            ("hide_inactive", ""),
            ("role", "system_admin"),
            ("team", "t1"),
            ("guest_filter", "all"),
            ("last_column_value", "bob"),
            ("last_user_id", "u9"),
        ]);
        let options = export_users_options(&data).unwrap();
        assert_eq!(options.base.start_at, 5);
        assert_eq!(options.base.end_at, 9);
        assert_eq!(options.base.sort_column, "Username");
        assert_eq!(options.base.page_size, 100);
        assert_eq!(options.base.from_column_value, "bob");
        assert_eq!(options.base.from_id, "u9");
        assert!(options.hide_active);
        assert!(!options.hide_inactive);
        assert_eq!(options.role, "system_admin");
        assert_eq!(options.team, "t1");
        assert_eq!(options.guest_filter, "all");

        assert!(
            export_users_options(&map(&[("end_at", "0")])).is_err(),
            "start_at is required"
        );
        assert!(
            export_users_options(&map(&[("start_at", "0")])).is_err(),
            "end_at is required"
        );
        assert!(
            export_users_options(&map(&[
                ("start_at", "0"),
                ("end_at", "0"),
                ("hide_active", "yes"),
            ]))
            .is_err(),
            "yes is not a Go bool"
        );
    }

    /// All four batch workers wait a second between batches, and are always enabled.
    #[test]
    fn the_batch_workers_run_a_batch_a_second() {
        for worker in [
            delete_empty_drafts_migration_worker(),
            delete_orphan_drafts_migration_worker(),
            delete_dms_preferences_migration_worker(),
            export_users_to_csv_worker(),
        ] {
            assert_eq!(worker.time_between_batches(), ONE_SECOND, "{worker:?}");
            assert!(worker.is_enabled(&Config::default()));
        }
        assert_eq!(
            export_users_to_csv_worker().with_job_type("x").job_type,
            "x"
        );
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
        let slot = WorkerSlot::new(cleanup_desktop_tokens_worker());
        assert!(slot.take(), "an idle slot takes the offer");
        assert!(slot.is_busy());
        assert!(!slot.take(), "a busy slot drops it");
        assert!(!slot.take(), "and keeps dropping it");
        slot.release();
        assert!(slot.take(), "a released slot takes the next one");
    }
}
