//! The three job reads: `getJobs` (api4/job.go:196), `getJob` (job.go:33) and `getJobsByType`
//! (job.go:269).
//!
//! # An empty list is `null` on one route and `[]` on the other
//!
//! `GET /api/v4/jobs?job_type=data_retention` answers the four bytes `null`;
//! `GET /api/v4/jobs/type/data_retention` answers `[]`. Same database, same zero rows, same
//! marshaller. The difference is the *store method's* slice initialiser — `var jobs []*model.Job`
//! versus `jobs := []*model.Job{}` — surviving all the way to `json.Marshal`. Both were measured
//! against the running server before this file was written; see [`encode_jobs`], which is the one
//! place the distinction is made, and [`mm_store::JobStore`] for the table.
//!
//! # And neither one has a trailing newline — except `getJob`, which does
//!
//! The two list handlers use `json.Marshal` + `w.Write`; `getJob` uses
//! `json.NewEncoder(w).Encode`. Three handlers in one file, two encoders, and the byte difference
//! is real ([D-086]).
//!
//! # `getJob` fetches before it checks
//!
//! The permission depends on `job.Type`, so there is nothing to check until the row is loaded.
//! The consequence is on the wire: a caller with no job permission at all gets **404** for an id
//! that does not exist and **403** for one that does, which is an existence oracle. It is Go's,
//! and it is reproduced rather than tightened.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::job::{self, Job};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM_ACCESS_RULES, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_first};
use crate::error::ApiError;

/// `RequireJobType`'s bound (web/context.go:650) — the only length check on any job parameter.
const JOB_TYPE_MAX_LEN: usize = 32;

/// Go's `{job_type:[A-Za-z0-9_-]+}` (api4/job.go:28) — the id class plus `_` and `-`, and
/// *without* the `.` the username class allows. A segment outside it never matches Go's route, so
/// it is forwarded and Go answers its own mux 404. [D-150]'s rule under a third alphabet.
fn segment_matches_job_type_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The body bytes for a list of jobs, with Go's nil-versus-empty distinction made explicit.
///
/// `nil_when_empty` is not a style choice — it selects between the four bytes `null` and the two
/// bytes `[]`, and which one is correct depends on which *store* method produced the list. Passing
/// the wrong value here is a silent wire divergence that no type can catch, which is why it is a
/// named parameter on a shared function rather than an `if` inside each handler.
fn encode_jobs(jobs: &[Job], nil_when_empty: bool) -> Result<Vec<u8>, ApiError> {
    if jobs.is_empty() && nil_when_empty {
        return Ok(b"null".to_vec());
    }
    serde_json::to_vec(jobs).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise jobs");
        ApiError::from(AppError::new(
            "getJobs",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// `api.job.retrieve.nopermissions` — the **400** Go answers when the job type has no permission
/// attached at all. Not a 403: the server is saying the type is unreadable by anyone, not that
/// this caller is refused.
///
/// `where` is `getJobsByType` on **both** routes that raise it, including `getJobs`
/// (api4/job.go:211) — Go's copy-paste, and `where` is on the wire.
fn no_permission_defined(where_: &'static str) -> ApiError {
    ApiError::from(AppError::new(
        where_,
        "api.job.retrieve.nopermissions",
        None,
        String::new(),
        400,
    ))
}

/// Port of `getJobs` (api4/job.go:196) — `GET /api/v4/jobs`.
///
/// # The permission filter is the query, not a gate
///
/// With no `job_type`, Go walks all 24 `AllJobTypes` and keeps the ones this session may read;
/// the resulting list becomes the `WHERE Type IN (…)`. A caller who may read nothing reaches
/// `len(validJobTypes) == 0` and gets a **403 naming no permission at all** —
/// `SetPermissionError()` with an empty variadic, so the detail ends in a bare `permission=`.
/// That is a distinct answer from the per-type 403 below and both are reproduced.
///
/// # The two branches use different store methods, and only one of them can answer `null`
///
/// No `status` → `GetJobsByTypesPage`, whose nil slice marshals as `null`. With a `status` →
/// `GetJobsByTypesAndStatuses`, whose `[]*model.Job{}` marshals as `[]`. So
/// `?status=success` and no status differ on an empty result — measured, and pinned by a test.
#[tracing::instrument(skip_all, fields(job_type, status, page, per_page, types, count))]
pub async fn get_jobs(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let requested_type = query_first(query.as_deref(), "job_type").unwrap_or_default();

    let valid_job_types: Vec<String> = if requested_type.is_empty() {
        let mut kept = Vec::new();
        for job_type in job::ALL_JOB_TYPES {
            let answer = state
                .app
                .session_has_permission_to_read_job(&session.0, job_type)
                .await;
            // Go logs a warning and *continues* — an unreadable type is skipped, not fatal.
            if answer.required().is_none() {
                tracing::warn!(
                    job_type,
                    "the job types of a job you are trying to retrieve does not contain permissions"
                );
                continue;
            }
            if answer.granted() {
                kept.push((*job_type).to_owned());
            }
        }
        kept
    } else {
        tracing::Span::current().record("job_type", &requested_type);
        if !job::is_valid_job_type(&requested_type) {
            return Err(ApiError::invalid_url_param("job_type"));
        }
        let answer = state
            .app
            .session_has_permission_to_read_job(&session.0, &requested_type)
            .await;
        let Some(required) = answer.required() else {
            return Err(no_permission_defined("getJobsByType"));
        };
        if !answer.granted() {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[required],
            )));
        }
        vec![requested_type]
    };

    tracing::Span::current().record("types", valid_job_types.len());

    if valid_job_types.is_empty() {
        // `c.SetPermissionError()` — no permissions, so the detail ends `permission=` with
        // nothing after it. A different body from every other 403 on this route.
        return Err(ApiError::from(make_permission_error(&session.0, &[])));
    }

    let status = query_first(query.as_deref(), "status").unwrap_or_default();
    if !status.is_empty() && !job::is_valid_job_status(&status) {
        return Err(ApiError::from(AppError::new(
            "getJobs",
            "api.job.status.invalid",
            None,
            String::new(),
            400,
        )));
    }
    tracing::Span::current().record("status", &status);

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    // The nil-versus-empty answer travels with the branch that chose the store method.
    let (jobs, nil_when_empty) = if status.is_empty() {
        (
            state
                .app
                .get_jobs_by_types_page(&valid_job_types, page, per_page)
                .await?,
            true,
        )
    } else {
        (
            state
                .app
                .get_jobs_by_types_and_statuses(&valid_job_types, &[status], page, per_page)
                .await?,
            false,
        )
    };
    tracing::Span::current().record("count", jobs.len());

    // `json.Marshal` then `w.Write` — no trailing newline.
    Ok(json_ok(encode_jobs(&jobs, nil_when_empty)?))
}

/// Port of `getJob` (api4/job.go:33) — `GET /api/v4/jobs/{job_id}`.
///
/// The only one of the three that uses `json.NewEncoder(w).Encode`, so this body — and only this
/// body — ends in a newline.
#[tracing::instrument(skip_all, fields(job_id = %job_id, job_type))]
pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireJobId` (web/context.go:634) — `invalid_url_param`, not the body-param id.
    if !is_valid_id(&job_id) {
        return Err(ApiError::invalid_url_param("job_id"));
    }

    // The fetch precedes the permission check because the permission depends on the type. See
    // the module note on the existence oracle this creates.
    let job = state.app.get_job(&job_id).await?;
    tracing::Span::current().record("job_type", &job.job_type);

    let answer = state
        .app
        .session_has_permission_to_read_job(&session.0, &job.job_type)
        .await;
    let Some(required) = answer.required() else {
        return Err(no_permission_defined("getJob"));
    };
    if !answer.granted() {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[required],
        )));
    }

    let mut body = serde_json::to_vec(&job).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise job");
        ApiError::from(AppError::new(
            "getJob",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok(json_ok(body))
}

/// Port of `getJobsByType` (api4/job.go:269) — `GET /api/v4/jobs/type/{job_type}`.
///
/// # Two grants that are not the type's own permission
///
/// A caller who fails `SessionHasPermissionToReadJob` can still be let through twice, and both
/// carve-outs are narrow enough that getting one condition wrong opens the route to the wrong
/// people:
///
/// - **`access_control_sync` + `team_id`** — team-scoped, and the team gate is
///   `manage_team_access_rules` **on the team in the query**.
/// - **`access_control_team_sync` + `policy_id` and _no_ `team_id`** — a team policy's id *is* its
///   team id, so the same team gate applies to `policy_id`. Go's own comment says the absent
///   `team_id` is what stops this grant authorising an unvetted team filter; that `!hasTeamFilter`
///   term is load-bearing, not defensive.
///
/// # `policy_id` without either grant is system-admin only
///
/// The `else if` branch re-checks `manage_system` and 403s naming *that* permission rather than
/// the type's — "to prevent job enumeration across policies" (job.go:332).
///
/// # The team and policy branches paginate in memory
///
/// `GetJobsByTypeAndData` has no `ORDER BY` and no `LIMIT`; Go sorts by `CreateAt` descending with
/// `sort.Slice` and slices the page out. `sort.Slice` is **not stable**, so jobs sharing a
/// millisecond have no defined order on Go's side. This sorts on `create_at` alone; Rust's
/// `sort_by_key` *is* stable, so a tie keeps store order here where Go's may not — the one place
/// the two can legitimately differ, and the parity suite compares such a page as a set. An id
/// tiebreak would be worse rather than better: it would make our order more defined than Go's.
#[tracing::instrument(skip_all, fields(job_type = %job_type, page, per_page, count, forwarded))]
pub async fn get_jobs_by_type(
    State(state): State<AppState>,
    Path(job_type): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if !segment_matches_job_type_mux(&job_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match get_jobs_by_type_inner(state, job_type, query, session).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn get_jobs_by_type_inner(
    state: AppState,
    job_type: String,
    query: Option<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireJobType` (web/context.go:645) — empty or over 32 characters. The mux charset above
    // already rejects the empty case, so only the length bound is reachable here.
    if job_type.is_empty() || job_type.len() > JOB_TYPE_MAX_LEN {
        return Err(ApiError::invalid_url_param("job_type"));
    }

    let answer = state
        .app
        .session_has_permission_to_read_job(&session.0, &job_type)
        .await;
    let Some(required) = answer.required() else {
        return Err(no_permission_defined("getJobsByType"));
    };
    let has_permission = answer.granted();

    let team_id = query_first(query.as_deref(), "team_id").unwrap_or_default();
    let has_team_filter = if team_id.is_empty() {
        false
    } else {
        if !is_valid_id(&team_id) {
            return Err(ApiError::invalid_url_param("team_id"));
        }
        true
    };

    let is_team_scoped_sync_request = !has_permission
        && job_type == job::JOB_TYPE_ACCESS_CONTROL_SYNC
        && has_team_filter
        && state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await;

    let policy_id = query_first(query.as_deref(), "policy_id").unwrap_or_default();
    let is_team_policy_scoped_sync_request = !has_permission
        && job_type == job::JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC
        && !has_team_filter
        && !policy_id.is_empty()
        && is_valid_id(&policy_id)
        && state
            .app
            .session_has_permission_to_team(
                &session.0,
                &policy_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await;

    if !has_permission && !is_team_scoped_sync_request && !is_team_policy_scoped_sync_request {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[required],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let jobs = if has_team_filter {
        let found = state
            .app
            .get_jobs_by_type_and_data(&job_type, "team_id", &team_id)
            .await?;
        page_in_memory(found, page, per_page)
    } else if !policy_id.is_empty()
        && (job_type == job::JOB_TYPE_ACCESS_CONTROL_SYNC
            || job_type == job::JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC)
    {
        if !is_team_policy_scoped_sync_request
            && !state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
                .await
        {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_MANAGE_SYSTEM],
            )));
        }
        let found = state
            .app
            .get_jobs_by_type_and_data(&job_type, "policy_id", &policy_id)
            .await?;
        page_in_memory(found, page, per_page)
    } else {
        state
            .app
            .get_jobs_by_type_page(&job_type, page, per_page)
            .await?
    };
    tracing::Span::current().record("count", jobs.len());

    // Every branch here produces a Go slice that was initialised non-nil, so an empty result is
    // `[]` and never `null` — the opposite of `getJobs`'s default branch.
    Ok(json_ok(encode_jobs(&jobs, false)?))
}

/// Go's `sort.Slice` + slice-the-page, for the two `GetJobsByTypeAndData` branches
/// (api4/job.go:386-398 and 344-353).
///
/// `start >= len` yields the empty slice rather than panicking, which is Go's explicit guard and
/// also what `Vec::drain` would need anyway.
fn page_in_memory(mut jobs: Vec<Job>, page: i64, per_page: i64) -> Vec<Job> {
    jobs.sort_by_key(|job| std::cmp::Reverse(job.create_at));

    let start = page.saturating_mul(per_page);
    if start >= jobs.len() as i64 {
        return Vec::new();
    }
    let start = start as usize;
    let end = (start + per_page.max(0) as usize).min(jobs.len());
    jobs[start..end].to_vec()
}

/// The 200 every handler here returns, with the cutover marker.
fn json_ok(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_at(create_at: i64) -> Job {
        Job {
            id: format!("job{create_at:0>23}"),
            job_type: job::JOB_TYPE_MIGRATIONS.to_owned(),
            priority: 0,
            create_at,
            start_at: 0,
            last_activity_at: 0,
            status: job::JOB_STATUS_SUCCESS.to_owned(),
            progress: 0,
            data: None,
        }
    }

    /// The whole point of `encode_jobs`: the same empty list is four bytes on one route and two
    /// on the other. Both were measured against the running Go server.
    #[test]
    fn an_empty_list_is_null_or_empty_depending_on_the_store_method() {
        assert_eq!(encode_jobs(&[], true).expect("encodes"), b"null".to_vec());
        assert_eq!(encode_jobs(&[], false).expect("encodes"), b"[]".to_vec());
    }

    /// A non-empty list ignores the flag entirely — `null` is only ever the *empty* answer.
    #[test]
    fn a_populated_list_is_an_array_either_way() {
        let jobs = vec![job_at(7)];
        let with = encode_jobs(&jobs, true).expect("encodes");
        let without = encode_jobs(&jobs, false).expect("encodes");
        assert_eq!(with, without);
        assert!(with.starts_with(b"[{"), "an array, not null");
    }

    /// Newest first, which is `CreateAt` **descending**. A port that sorted ascending would
    /// return the oldest page of jobs and still look plausible.
    #[test]
    fn in_memory_pagination_sorts_newest_first() {
        let sorted = page_in_memory(vec![job_at(10), job_at(30), job_at(20)], 0, 60);
        let order: Vec<i64> = sorted.iter().map(|j| j.create_at).collect();
        assert_eq!(order, vec![30, 20, 10]);
    }

    /// The page window, and the guard Go writes explicitly: a start past the end is `[]`, not a
    /// panic and not the last page.
    #[test]
    fn in_memory_pagination_windows_and_runs_out() {
        let all = || vec![job_at(10), job_at(30), job_at(20), job_at(40)];

        let first: Vec<i64> = page_in_memory(all(), 0, 2)
            .iter()
            .map(|j| j.create_at)
            .collect();
        assert_eq!(first, vec![40, 30]);

        let second: Vec<i64> = page_in_memory(all(), 1, 2)
            .iter()
            .map(|j| j.create_at)
            .collect();
        assert_eq!(second, vec![20, 10]);

        assert!(page_in_memory(all(), 2, 2).is_empty(), "past the end is []");
        // A partial last page is truncated at the length, not padded.
        let partial: Vec<i64> = page_in_memory(all(), 1, 3)
            .iter()
            .map(|j| j.create_at)
            .collect();
        assert_eq!(partial, vec![10]);
    }

    /// Go's job-type mux class is the id class plus `_` and `-` — and **not** `.`, which the
    /// username class two files away does allow. A near-miss must be forwarded, not answered.
    #[test]
    fn the_job_type_charset_is_gos_mux_class() {
        for ok in ["data_retention", "access-control", "Migrations9", "a"] {
            assert!(segment_matches_job_type_mux(ok), "{ok:?} matches Go's mux");
        }
        for bad in [
            "",
            "data.retention",
            "data retention",
            "data+retention",
            "café",
        ] {
            assert!(
                !segment_matches_job_type_mux(bad),
                "{bad:?} falls to Go's mux 404"
            );
        }
    }

    /// The 400 that is *not* a 403. Both routes that raise it report `getJobsByType` as `where`,
    /// including `getJobs` — Go's copy-paste, and `where` is on the wire.
    #[test]
    fn an_undefined_job_type_is_a_400_not_a_403() {
        let err = no_permission_defined("getJobsByType");
        assert_eq!(err.0.status_code, 400);
        assert_eq!(err.0.id, "api.job.retrieve.nopermissions");
        assert_eq!(err.0.where_, "getJobsByType");
    }
}
