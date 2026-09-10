//! The two reads of `channels/api4/report.go`: `getUsersForReporting` (report.go:26) at
//! `GET /api/v4/reports/users` and `getUserCountForReporting` (report.go:58) at
//! `GET /api/v4/reports/users/count`.
//!
//! # The same query string, read two different ways
//!
//! Both handlers call `fillUserReportOptions`, which reads the six *filter* parameters and is the
//! only place either of them can answer 400 on a parameter. Only the list route also calls
//! `fillReportingBaseOptions`, which reads the seven *pagination and date-range* parameters. So
//! `?sort_column=nonsense` is a 400 on the list and silently ignored on the count, and
//! `?date_range=last_30_days` narrows the aggregates on the list and does nothing at all to the
//! count — the date range only ever reaches the `PostStats` join, which the count query has not
//! got. See [`mm_store::UserStore::get_user_count_for_report`].
//!
//! # Every parameter is optional and every parse failure is silent
//!
//! `page_size` goes through `strconv.ParseInt` with the error discarded, so `?page_size=abc` is
//! 50 and not a 400. `direction` is `next` unless the value is exactly `prev`. `sort_direction`
//! is descending only for exactly `desc`. The three booleans compare against the literal string
//! `"true"` — `?hide_active=1` is **false**, which is not `strconv.ParseBool` and not what the
//! rest of the API does.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS, make_permission_error,
};
use mm_model::report::{REPORTING_MAX_PAGE_SIZE, ReportingBaseOptions, UserReportOptions};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::query_first;
use crate::error::ApiError;

/// Port of `getUsersForReporting` (report.go:26).
///
/// # The page-size bound is checked *after* the filter parameters
///
/// Go fills the base options, then the user options — returning that function's 400 if it has one
/// — and only then rejects a page size outside `1..=100`. So a request that is wrong in both ways
/// reports the filter error, and a mutation that moves the bound check earlier changes which id a
/// client sees. There is a test for the order.
///
/// # `page_size` has no upper default
///
/// `ReportingMaxPageSize` is 100 and the *default* is 50, so the only way to reach the bound is
/// to ask for it. Note that the store would happily accept a larger limit — the comment in Go
/// says "Don't allow fetching more than 100 users at a time **from the normal query endpoint**",
/// and the export route next door passes the same options with no bound at all.
#[tracing::instrument(skip_all, fields(page_size, count))]
pub async fn get_users_for_reporting(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        )));
    }

    let base = fill_reporting_base_options(query.as_deref());
    let mut options = fill_user_report_options(query.as_deref())?;
    options.base = base;

    tracing::Span::current().record("page_size", options.base.page_size);
    if options.base.page_size <= 0 || options.base.page_size > REPORTING_MAX_PAGE_SIZE {
        return Err(ApiError(AppError::boxed(
            "getUsersForReporting",
            "api.getUsersForReporting.invalid_page_size",
            None,
            String::new(),
            400,
        )));
    }

    let reports = state.app.get_users_for_reporting(&options).await?;
    tracing::Span::current().record("count", reports.len());

    encode(&reports, "getUsersForReporting")
}

/// Port of `getUserCountForReporting` (report.go:58).
///
/// The body is `json.NewEncoder(w).Encode(count)` on a `*int64` — a bare number and a newline.
#[tracing::instrument(skip_all, fields(count))]
pub async fn get_user_count_for_reporting(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        )));
    }

    // No `fillReportingBaseOptions` on this route: `options.ReportingBaseOptions` stays at its
    // zero value, so the count is never paginated, never sorted and never date-bounded.
    let options = fill_user_report_options(query.as_deref())?;

    let count = state.app.get_user_count_for_report(&options).await?;
    tracing::Span::current().record("count", count);

    encode(&count, "getUserCountForReporting")
}

/// Port of `fillReportingBaseOptions` (report.go:107).
///
/// `PopulateDateRange(time.Now())` runs here rather than in the store, so the window is pinned to
/// the instant the request arrived.
fn fill_reporting_base_options(query: Option<&str>) -> ReportingBaseOptions {
    let get = |key: &str| query_first(query, key).unwrap_or_default();

    let sort_column = match get("sort_column") {
        column if column.is_empty() => "Username".to_owned(),
        column => column,
    };

    let mut options = ReportingBaseOptions {
        // Anything that is not exactly `prev` is `next`, including the empty string.
        direction: if get("direction") == "prev" {
            "prev".to_owned()
        } else {
            "next".to_owned()
        },
        sort_column,
        sort_desc: get("sort_direction") == "desc",
        // `strconv.ParseInt(…, 10, 64)` with the error discarded: an absent or unparseable
        // `page_size` is 50, and a **negative** one survives to be rejected by the bound check.
        page_size: get("page_size").parse::<i64>().unwrap_or(50),
        from_column_value: get("from_column_value"),
        from_id: get("from_id"),
        date_range: get("date_range"),
        start_at: 0,
        end_at: 0,
    };
    options.populate_date_range(chrono::Local::now());
    options
}

/// Port of `fillUserReportOptions` (report.go:132).
///
/// Both errors name **`getUsersForReporting`** as the caller even when the count route raised
/// them; that is Go's copy-paste and it is on the wire, so it is reproduced.
fn fill_user_report_options(query: Option<&str>) -> Result<UserReportOptions, ApiError> {
    let get = |key: &str| query_first(query, key).unwrap_or_default();

    let team_filter = get("team_filter");
    if !(team_filter.is_empty() || is_valid_id(&team_filter)) {
        return Err(ApiError(AppError::boxed(
            "getUsersForReporting",
            "api.getUsersForReporting.invalid_team_filter",
            None,
            String::new(),
            400,
        )));
    }

    // `values.Get("hide_active") == "true"` — a string comparison, not `ParseBool`.
    let hide_active = get("hide_active") == "true";
    let hide_inactive = get("hide_inactive") == "true";
    if hide_active && hide_inactive {
        return Err(ApiError(AppError::boxed(
            "getUsersForReporting",
            "api.getUsersForReporting.invalid_active_filter",
            None,
            String::new(),
            400,
        )));
    }

    Ok(UserReportOptions {
        base: ReportingBaseOptions::default(),
        team: team_filter,
        role: get("role_filter"),
        has_no_team: get("has_no_team") == "true",
        hide_active,
        hide_inactive,
        search_term: get("search_term"),
        guest_filter: get("guest_filter"),
    })
}

/// `json.NewEncoder(w).Encode(v)` — the body plus the encoder's trailing newline.
fn encode<T: serde::Serialize>(value: &T, caller: &'static str) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise a user report");
        ApiError(AppError::boxed(
            caller,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}
