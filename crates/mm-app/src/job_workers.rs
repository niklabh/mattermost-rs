//! The `SimpleWorker` bodies ported after `cleanup_desktop_tokens` (D-804), one function per Go
//! `jobs/<type>/worker.go`. The runtime that claims and records a job is `crate::job_runtime`;
//! this module only supplies what each worker's `isEnabled` and `execute` do, and each is added to
//! `registered_workers` there.
//!
//! # Two of these do nothing on any build of this tree, and are ported anyway
//!
//! `active_users` and `mobile_session_metadata` feed `einterfaces.MetricsInterface`, which only
//! the enterprise build registers. On every server built from the public tree `GetMetrics()` is
//! nil, so Go's `active_users` runs its count and discards it, and `mobile_session_metadata`
//! returns before touching the store. What remains observable is the job row: claimed, then
//! `success` at progress 100 — or `error` when the count fails. That is what is reproduced.

use mm_model::job;
use mm_model::user_count::UserCountOptions;
use mm_store::{FileInfoStore, PostStore, UserStore};

use std::time::Duration;

use chrono::NaiveTime;

use crate::job_runtime::{SimpleWorker, WorkerError};
use crate::job_scheduler::{DailyScheduler, PeriodicScheduler};

/// Port of `jobs/active_users/worker.go`: `User().Count(UserCountOptions{IncludeDeleted:
/// false})`, handed to a metrics interface that is nil here (see the module note). Enabled by
/// `MetricsSettings.Enable`.
pub fn active_users_worker() -> SimpleWorker {
    SimpleWorker::new(
        "ActiveUsers",
        job::JOB_TYPE_ACTIVE_USERS,
        |config| config.metrics_enable,
        |app, _job| {
            Box::pin(async move {
                let options = UserCountOptions {
                    include_deleted: false,
                    ..UserCountOptions::default()
                };
                let count = app
                    .store()
                    .user()
                    .count(&options)
                    .await
                    .map_err(WorkerError::from)?;
                tracing::debug!(
                    count,
                    "active users counted; no metrics interface to observe it"
                );
                Ok(())
            })
        },
    )
}

/// Port of `jobs/mobile_session_metadata/worker.go`: `if metrics == nil { return nil }` comes
/// before `GetMobileSessionMetadata`, and the metrics interface is nil here, so the body is the
/// early return. Enabled by `MetricsSettings.EnableClientMetrics`.
pub fn mobile_session_metadata_worker() -> SimpleWorker {
    SimpleWorker::new(
        "MobileSessionMetadata",
        job::JOB_TYPE_MOBILE_SESSION_METADATA,
        |config| config.metrics_enable_client_metrics,
        |_app, _job| Box::pin(async move { Ok(()) }),
    )
}

/// Port of `jobs/refresh_materialized_views/worker.go`: `RefreshPostStats` (two views), then
/// `RefreshFileStats`, then `RefreshPostStatsForUsers` — the first failure ends the job. Enabled
/// when `SqlSettings.DriverName` is `postgres`, which this server always is.
pub fn refresh_materialized_views_worker() -> SimpleWorker {
    SimpleWorker::new(
        "RefreshMaterializedViews",
        job::JOB_TYPE_REFRESH_MATERIALIZED_VIEWS,
        |_config| true,
        |app, _job| {
            Box::pin(async move {
                let timeout = app.config().analytics_query_timeout;
                let store = app.store();
                store
                    .post()
                    .refresh_post_stats(timeout)
                    .await
                    .map_err(WorkerError::from)?;
                store
                    .file_info()
                    .refresh_file_stats(timeout)
                    .await
                    .map_err(WorkerError::from)?;
                store
                    .user()
                    .refresh_post_stats_for_users(timeout)
                    .await
                    .map_err(WorkerError::from)
            })
        },
    )
}

/// Port of `jobs/expirynotify/worker.go`: [`crate::App::notify_sessions_expired`]. Enabled by
/// `ServiceSettings.ExtendSessionLengthWithActivity`.
pub fn expiry_notify_worker() -> SimpleWorker {
    SimpleWorker::new(
        "ExpiryNotify",
        job::JOB_TYPE_EXPIRY_NOTIFY,
        |config| config.extend_session_length_with_activity,
        |app, _job| {
            Box::pin(async move {
                app.notify_sessions_expired()
                    .await
                    .map_err(|err| -> WorkerError { err })
            })
        },
    )
}

/// Port of `jobs/cleanup_expired_access_tokens/worker.go`:
/// [`crate::App::cleanup_expired_access_tokens`] with the cut-off at the time the job runs and
/// Go's 1000 × 1000 bounds. Enabled by `ServiceSettings.EnableUserAccessTokens`.
pub fn cleanup_expired_access_tokens_worker() -> SimpleWorker {
    SimpleWorker::new(
        "CleanupExpiredAccessTokens",
        job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS,
        |config| config.enable_user_access_tokens,
        |app, _job| {
            Box::pin(async move {
                app.cleanup_expired_access_tokens(
                    mm_model::utils::get_millis(),
                    crate::access_token_expiry::CLEANUP_BATCH_LIMIT,
                    crate::access_token_expiry::CLEANUP_MAX_BATCHES,
                )
                .await
                .map_err(WorkerError::from)
            })
        },
    )
}

/// Port of `jobs/notify_expiring_access_tokens/worker.go`:
/// [`crate::App::notify_expiring_access_tokens`]. Enabled by
/// `ServiceSettings.EnableUserAccessTokens`.
pub fn notify_expiring_access_tokens_worker() -> SimpleWorker {
    SimpleWorker::new(
        "NotifyExpiringAccessTokens",
        job::JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS,
        |config| config.enable_user_access_tokens,
        |app, _job| {
            Box::pin(async move {
                app.notify_expiring_access_tokens()
                    .await
                    .map_err(|err| -> WorkerError { err })
            })
        },
    )
}

// ---------------------------------------------------------------------------
// The schedulers that queue these workers' jobs (registered, not started — D-802)
// ---------------------------------------------------------------------------

/// Port of `jobs/active_users/scheduler.go`: every ten minutes, while `MetricsSettings.Enable`.
pub fn active_users_scheduler() -> PeriodicScheduler {
    PeriodicScheduler::new(
        job::JOB_TYPE_ACTIVE_USERS,
        Duration::from_secs(10 * 60),
        |config| config.metrics_enable,
    )
}

/// Port of `jobs/mobile_session_metadata/scheduler.go`: daily, while
/// `MetricsSettings.EnableClientMetrics`.
pub fn mobile_session_metadata_scheduler() -> PeriodicScheduler {
    PeriodicScheduler::new(
        job::JOB_TYPE_MOBILE_SESSION_METADATA,
        Duration::from_secs(24 * 60 * 60),
        |config| config.metrics_enable_client_metrics,
    )
}

/// Port of `jobs/refresh_materialized_views/scheduler.go` — the public tree's only
/// `DailyScheduler`: at `ServiceSettings.RefreshPostStatsRunTime` each day, a start time that
/// does not parse as Go's `"15:04"` being the `nil` that switches it off.
pub fn refresh_materialized_views_scheduler() -> DailyScheduler {
    DailyScheduler::new(
        job::JOB_TYPE_REFRESH_MATERIALIZED_VIEWS,
        |config| parse_go_hhmm(&config.refresh_post_stats_run_time),
        |_config| true,
    )
}

/// Port of `jobs/expirynotify/scheduler.go`: every ten minutes, while
/// `ServiceSettings.ExtendSessionLengthWithActivity`.
pub fn expiry_notify_scheduler() -> PeriodicScheduler {
    PeriodicScheduler::new(
        job::JOB_TYPE_EXPIRY_NOTIFY,
        Duration::from_secs(10 * 60),
        |config| config.extend_session_length_with_activity,
    )
}

/// Port of `jobs/cleanup_expired_access_tokens/scheduler.go`: hourly, while
/// `ServiceSettings.EnableUserAccessTokens`.
pub fn cleanup_expired_access_tokens_scheduler() -> PeriodicScheduler {
    PeriodicScheduler::new(
        job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS,
        Duration::from_secs(60 * 60),
        |config| config.enable_user_access_tokens,
    )
}

/// Port of `jobs/notify_expiring_access_tokens/scheduler.go`: hourly, while
/// `ServiceSettings.EnableUserAccessTokens`.
pub fn notify_expiring_access_tokens_scheduler() -> PeriodicScheduler {
    PeriodicScheduler::new(
        job::JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS,
        Duration::from_secs(60 * 60),
        |config| config.enable_user_access_tokens,
    )
}

/// `time.Parse("15:04", input)`, keeping only the hour and minute — the daily scheduler reads
/// nothing else (see `crate::job_scheduler::generate_next_start_date_time`).
///
/// Go's layout elements, not chrono's `%H:%M`, which accepts a one-digit minute Go refuses
/// (`"03:0"`, [D-803]):
///
/// - `15` (`stdHour`) is one **or** two ASCII digits, 0–23;
/// - `:` is literal;
/// - `04` (`stdZeroMinute`) is **exactly** two ASCII digits, 0–59;
/// - anything left over is Go's "extra text" error.
///
/// No whitespace is skipped and no sign is taken; a non-ASCII digit is not a digit.
pub fn parse_go_hhmm(input: &str) -> Option<NaiveTime> {
    let bytes = input.as_bytes();
    let digit = |i: usize| {
        bytes
            .get(i)
            .filter(|b| b.is_ascii_digit())
            .map(|b| u32::from(b - b'0'))
    };

    let first = digit(0)?;
    let (hour, rest) = match digit(1) {
        Some(second) => (first * 10 + second, 2),
        None => (first, 1),
    };
    if bytes.get(rest) != Some(&b':') {
        return None;
    }
    let minute = digit(rest + 1)? * 10 + digit(rest + 2)?;
    if bytes.len() != rest + 3 {
        return None;
    }
    // Go's "hour out of range" and "minute out of range": `from_hms_opt` refuses 24 and 60.
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Each `isEnabled` reads its own setting, and the defaults are Go's: metrics off, client
    /// metrics on, the refresh unconditional.
    #[test]
    fn each_worker_is_enabled_by_its_own_setting() {
        let default = Config::default();
        assert!(!active_users_worker().is_enabled(&default));
        assert!(mobile_session_metadata_worker().is_enabled(&default));
        assert!(refresh_materialized_views_worker().is_enabled(&default));

        let flipped = Config {
            metrics_enable: true,
            metrics_enable_client_metrics: false,
            ..Config::default()
        };
        assert!(active_users_worker().is_enabled(&flipped));
        assert!(!mobile_session_metadata_worker().is_enabled(&flipped));
    }

    /// The registry key is the job type, never the log name.
    #[test]
    fn the_workers_claim_gos_job_types() {
        assert_eq!(active_users_worker().job_type, "active_users");
        assert_eq!(
            mobile_session_metadata_worker().job_type,
            "mobile_session_metadata"
        );
        assert_eq!(
            refresh_materialized_views_worker().job_type,
            "refresh_materialized_views"
        );
    }
}
