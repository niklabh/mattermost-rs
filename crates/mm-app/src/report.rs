//! Port of `App.GetUsersForReporting` and `App.GetUserCountForReport` (channels/app/report.go:176
//! and :194) — the two reads behind the System Console's *User Management → Users* report.
//!
//! # Only one of the two validates its options
//!
//! `GetUsersForReporting` calls `filter.IsValid()` first, so a bad sort column or guest filter is
//! a **400** with a `model.user_report_options.is_valid.*` id. `GetUserCountForReport` does not
//! call it at all — and its handler never fills `ReportingBaseOptions` either, so the count route
//! has no sort column to reject. The asymmetry is Go's; both halves are reproduced.

use mm_model::report::{UserReport, UserReportOptions};
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, UserStore};

use crate::App;

impl App {
    /// Port of `App.GetUsersForReporting` (report.go:176).
    ///
    /// The rows come back as `UserReportQuery` and are converted one at a time by
    /// `ToReport()`, which **sanitises the embedded user** with `ClearNonProfileFields(true)`
    /// before it copies it. That is the only sanitisation on this path: the handler does not call
    /// `SanitizeProfile`, so what the store read is what `ClearNonProfileFields` leaves.
    ///
    /// An empty result is `make([]*model.UserReport, 0)` — a non-nil empty slice, so the route
    /// answers `[]` and not `null`.
    #[tracing::instrument(skip_all, fields(page_size = options.base.page_size, found))]
    pub async fn get_users_for_reporting(
        &self,
        options: &UserReportOptions,
    ) -> AppResult<Vec<UserReport>> {
        options.is_valid()?;

        let rows = self
            .store()
            .user()
            .get_user_report(options)
            .await
            .map_err(|err| report_error("GetUsersForReporting", "get_user_report", err))?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(|mut row| row.to_report()).collect())
    }

    /// Port of `App.GetUserCountForReport` (report.go:194).
    ///
    /// Go returns `*int64` and the handler encodes the pointer, so the body is a bare JSON
    /// number — never an object and never `null`, since the pointer is only nil on the error
    /// path the handler has already returned from.
    #[tracing::instrument(skip_all, fields(count))]
    pub async fn get_user_count_for_report(&self, options: &UserReportOptions) -> AppResult<i64> {
        let count = self
            .store()
            .user()
            .get_user_count_for_report(options)
            .await
            .map_err(|err| {
                report_error("GetUserCountForReport", "get_user_count_for_report", err)
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

/// Both app functions collapse **every** store failure into one 500 — there is no `ErrNotFound`
/// branch and no out-of-bounds branch, so a malformed cursor and a dead database are the same
/// response. The `where` clause is the only thing that differs between them.
fn report_error(caller: &'static str, where_: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "user report query failed");
    AppError::boxed(
        caller,
        format!("app.report.{where_}.store_error"),
        None,
        String::new(),
        500,
    )
}
