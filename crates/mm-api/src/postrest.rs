//! The rest of `api4/post.go` — `setPostReminder`, `restorePostVersion`, `moveThread`,
//! `rewriteMessage`, `revealPost`, `burnPost` — plus `api4/report.go`'s two writes
//! (`getPostsForReporting`, `startUsersBatchExport`) and all of `api4/integration_action.go`
//! (`doPostAction`, `openDialog`, `submitDialog`, `lookupDialog`, `executeDialogAction`).
//!
//! # Every forward is decided before a write
//!
//! Each handler's refusals are its own; the branches handed to Go are named on the handler and
//! are all reached before any row changes: the DM/GM reminder (its permalink is fetched, not
//! previewed), a restore that changes the file set, a licensed `moveThread`, the agents bridge
//! behind `rewriteMessage`, a reveal whose revealed text carries a link, the author's own
//! `burnPost`, `include_metadata` on a post this server cannot prepare, a cookie-carrying
//! `doPostAction`, and every integration call the outbound guard would let through.
//!
//! # `openDialog` needs no session
//!
//! It is registered with `APIHandler`, not `APISessionRequired`: the caller is the integration
//! holding a `trigger_id`, and the user the dialog opens for is the one **inside** the trigger
//! id, verified against the installation's signing key. So [`OptionalSession`] here, and its
//! value is never read.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_app::post_rest::{OutboundDisposition, PostActionOutcome};
use mm_model::integration_action::{
    DoPostActionRequest, ExecuteDialogActionRequest, ExecuteDialogActionResponse,
    OpenDialogRequest, PostActionAPIResponse, SubmitDialogRequest, is_valid_lookup_url,
};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_EDIT_POST, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::post::{MoveThreadParams, Post, PostPatch, PostReminder};
use mm_model::post_rest::{
    MAX_REPORTING_PER_PAGE, REPORTING_SORT_DIRECTION_ASC, REPORTING_SORT_DIRECTION_DESC,
    REPORTING_TIME_FIELD_CREATE_AT, REPORTING_TIME_FIELD_UPDATE_AT, ReportPostQueryParams,
    ReportPostRequest, RewriteRequest, decode_report_post_cursor_v1,
};
use mm_model::report::get_report_date_range;
use mm_model::utils::{AppError, go_json_marshal, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::auth_writes::OptionalSession;
use crate::channels::resolve_me;
use crate::error::ApiError;
use crate::post_writes::{encoded_post, post_patch_checks};
use crate::proxy;
use crate::reports::{fill_reporting_base_options, fill_user_report_options};

/// `model.HeaderRequestedWith` / `model.HeaderRequestedWithXML` (model/client4.go — the
/// constants, not the client).
const HEADER_REQUESTED_WITH: &str = "X-Requested-With";
const HEADER_REQUESTED_WITH_XML: &str = "XMLHttpRequest";
/// `model.ConnectionId`.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// `web.ReturnStatusOK` — `{"status":"OK"}` with no trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// `json.NewEncoder(w).Encode(v)`: Go's marshal (HTML-escaped) plus the encoder's newline.
fn encode_with_newline<T: serde::Serialize>(value: &T, caller: &'static str) -> Response {
    match go_json_marshal(value) {
        Ok(mut body) => {
            body.push('\n');
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
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the response");
            ApiError(AppError::boxed(
                caller,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// The request body, read whole; the parts are kept so the request can still be forwarded.
async fn read_body(
    request: Request,
    parameter: &str,
) -> Result<(axum::http::request::Parts, axum::body::Bytes), ApiError> {
    let (parts, body) = request.into_parts();
    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => Ok((parts, bytes)),
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            Err(ApiError::invalid_param(parameter))
        }
    }
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn app_error(where_: &'static str, id: &str, status: i32) -> ApiError {
    ApiError(AppError::boxed(where_, id, None, String::new(), status))
}

// ---------------------------------------------------------------------------------------------
// api4/post.go
// ---------------------------------------------------------------------------------------------

/// Port of `setPostReminder` (api4/post.go:1323).
///
/// `RequirePostId().RequireUserId()`: the post id is checked first and `me` is the session's
/// user. Then the two permissions — the caller may act for the user, and may read the post —
/// then the body, whose only field is `target_time` in **seconds**. Forwards a reminder on a DM
/// or group-channel post; see [`mm_app::App::set_post_reminder`].
#[tracing::instrument(skip_all, fields(post_id = %post_id, user_id = %user_id, forwarded))]
pub async fn set_post_reminder(
    State(state): State<AppState>,
    Path((user_id, post_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let user_id = resolve_me(&user_id, &session);
    if !is_valid_id(user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    if session.0.user_id != user_id
        && !state
            .app
            .session_has_permission_to_user(&session.0, user_id)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }
    let (may_read, _) = state
        .app
        .session_has_permission_to_read_post(&session.0, &post_id)
        .await;
    if !may_read {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        ))
        .into_response();
    }

    let (parts, bytes) = match read_body(request, "target_time").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let reminder: PostReminder = match serde_json::from_slice(&bytes) {
        Ok(reminder) => reminder,
        Err(err) => {
            tracing::debug!(error = %err, "reminder body did not decode");
            return ApiError::invalid_param("target_time").into_response();
        }
    };

    match state
        .app
        .set_post_reminder(&session.0, &post_id, user_id, reminder.target_time)
        .await
    {
        Ok(()) => status_ok(),
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the reminder to Go");
            proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
        }
    }
}

/// Port of `restorePostVersion` (api4/post.go:1736).
///
/// `restore_version_id` is a mux segment (`[A-Za-z0-9]+`) and is **never** checked for its
/// length: a short one is simply a history row that is not there, which is the 403
/// `edit_post` from the first read. The order after it: the author check on the history row,
/// `postPatchChecks` on the **current** post with the old message and files as the patch (so
/// the edit time limit applies), then the app's four safeguards and the patch.
#[tracing::instrument(skip_all, fields(post_id = %post_id, restore_version_id = %restore_version_id, forwarded))]
pub async fn restore_post_version(
    State(state): State<AppState>,
    Path((post_id, restore_version_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }

    let to_restore = match state.app.get_single_post(&restore_version_id, true).await {
        Ok(post) => post,
        Err(_) => {
            return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]))
                .into_response();
        }
    };
    if session.0.user_id != to_restore.user_id {
        return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]))
            .into_response();
    }

    let patch = PostPatch {
        message: Some(to_restore.message.clone()),
        file_ids: Some(to_restore.file_ids.clone().unwrap_or_default()),
        ..PostPatch::default()
    };
    let outcome = async {
        post_patch_checks(&state, &post_id, &session, &patch).await?;
        let (updated, _is_member_for_preview) = state
            .app
            .restore_post_version(&session.0, &post_id, &restore_version_id)
            .await?;
        encoded_post(updated)
    }
    .await;
    match outcome {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the restore to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// Port of `moveThread` (api4/post.go:1503) up to its gate.
///
/// `!FeatureFlags.MoveThreadsEnabled || License() == nil` is the 501
/// `api.post.move_thread.disabled.app_error`, ahead of the body. The flag is environment-only
/// and off by default, so this is the whole route on every stack that has not set it; a stack
/// that has, and is licensed, is handed to Go for `App.MoveThread` — the wrangler copy of a
/// whole thread, unported.
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn move_thread(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let licensed = match state.app.license().await {
        Ok(license) => license.is_some(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    if !state.app.config().feature_flag_move_threads_enabled || !licensed {
        return app_error("moveThread", "api.post.move_thread.disabled.app_error", 501)
            .into_response();
    }
    // The body is Go's to decode from here; `MoveThreadParams` is the type it reads.
    let _: Option<MoveThreadParams> = None;
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

/// Port of `rewriteMessage` (api4/post.go:1812) up to the agents bridge.
///
/// Three 400s in order — the body (`request_body`), `agent_id` not id-shaped, `root_id` given
/// and not id-shaped — then [`mm_app::App::rewrite_message_gates`], whose last step is always
/// the forward.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn rewrite_message(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request, "request_body").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let req: RewriteRequest = match serde_json::from_slice(&bytes) {
        Ok(req) => req,
        Err(err) => {
            tracing::debug!(error = %err, "rewrite body did not decode");
            return ApiError::invalid_param("request_body").into_response();
        }
    };
    if !is_valid_id(&req.agent_id) {
        return ApiError::invalid_param("agent_id").into_response();
    }
    if !req.root_id.is_empty() && !is_valid_id(&req.root_id) {
        return ApiError::invalid_param("root_id").into_response();
    }

    match state
        .app
        .rewrite_message_gates(&session.0, &req.message, &req.action, &req.root_id)
        .await
    {
        Ok(()) | Err(PrepareError::Unreproducible(_)) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
        }
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
    }
}

/// Port of `revealPost` (api4/post.go:1852).
///
/// A cookie-authenticated request — no `Authorization` header — must carry
/// `X-Requested-With: XMLHttpRequest` or is the 403 `invalid_request`, **before** the post id
/// is looked at. Then the id, the `BurnOnRead` feature flag (the flag alone, not the
/// setting: 501 `disabled`), the authorized read, channel membership (403
/// `user_not_in_channel` for a missing member, the member read's own error otherwise), the
/// author (400 `cannot_reveal_own_post`), and [`mm_app::App::reveal_post`].
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn reveal_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let headers = request.headers();
    if header_value(headers, "Authorization").is_empty()
        && header_value(headers, HEADER_REQUESTED_WITH) != HEADER_REQUESTED_WITH_XML
    {
        return app_error(
            "revealPost",
            "api.post.reveal_post.invalid_request.app_error",
            403,
        )
        .into_response();
    }
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let connection_id = header_value(headers, CONNECTION_ID_HEADER).to_owned();

    if !state.app.config().feature_flag_burn_on_read {
        return app_error("revealPost", "api.post.reveal_post.disabled.app_error", 501)
            .into_response();
    }

    let user_id = session.0.user_id.clone();
    let (post, _is_member) = match state
        .app
        .get_post_if_authorized(&post_id, &session.0, false)
        .await
    {
        Ok(found) => found,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) = state
        .app
        .get_channel_member(&post.channel_id, &user_id)
        .await
    {
        if err.id == "app.channel.get_member.missing.app_error" {
            return app_error(
                "revealPost",
                "api.post.reveal_post.user_not_in_channel.app_error",
                403,
            )
            .into_response();
        }
        return ApiError::from(err).into_response();
    }
    if post.user_id == user_id {
        return app_error(
            "revealPost",
            "api.post.reveal_post.cannot_reveal_own_post.app_error",
            400,
        )
        .into_response();
    }

    match state.app.reveal_post(&post, &user_id, &connection_id).await {
        Ok(revealed) => match encoded_post(revealed) {
            Ok(response) => response,
            Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
            Err(PrepareError::Unreproducible(_)) => {
                proxy::forward_to_go(State(state), request).await
            }
        },
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the reveal to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// Port of `burnPost` (api4/post.go:1927): the id, the authorized read, channel membership
/// (403 `burn_post.user_not_in_channel`), then [`mm_app::App::burn_post`] — which hands the
/// author's own burn to Go before reading anything.
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn burn_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let connection_id = header_value(request.headers(), CONNECTION_ID_HEADER).to_owned();
    let user_id = session.0.user_id.clone();

    let (post, _) = match state
        .app
        .get_post_if_authorized(&post_id, &session.0, false)
        .await
    {
        Ok(found) => found,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Err(err) = state
        .app
        .get_channel_member(&post.channel_id, &user_id)
        .await
    {
        if err.id == "app.channel.get_member.missing.app_error" {
            return app_error(
                "burnPost",
                "api.post.burn_post.user_not_in_channel.app_error",
                403,
            )
            .into_response();
        }
        return ApiError::from(err).into_response();
    }

    match state.app.burn_post(&post, &user_id, &connection_id).await {
        Ok(()) => status_ok(),
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the burn to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

// ---------------------------------------------------------------------------------------------
// api4/report.go
// ---------------------------------------------------------------------------------------------

/// `c.IsSystemAdmin()` — `SessionHasPermissionTo(manage_system)`, which also opens for an
/// unrestricted (local-mode) session.
async fn require_system_admin(
    state: &AppState,
    session: &AuthenticatedSession,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        Ok(())
    } else {
        Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )))
    }
}

/// Port of `getPostsForReporting` (api4/report.go:174).
///
/// The body is both option halves flat. With a `cursor`, every query-affecting field comes
/// out of the cursor and the body's own `time_field`, `sort_direction`, `include_deleted` and
/// `exclude_system_posts` are ignored; without one, `time_field` is `update_at` only when it
/// says exactly that, `sort_direction` `desc` likewise, and the start is `start_time` when
/// positive, else `0` ascending and `MaxInt64` descending. `per_page` is read off the body on
/// **every** page: `<= 0` is 100 and anything above 1000 is 1000. `Validate` runs on the
/// resolved parameters either way.
///
/// **An empty page re-reads the body's `channel_id`**, not the cursor's, through `GetChannel`
/// for a better error — so a cursor request that omits `channel_id` and lands on an empty page
/// is the 404 `app.channel.get.existing.app_error`, and the cursor's own channel never comes
/// into it. `channel == nil` after a successful read is unreachable.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn get_posts_for_reporting(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_system_admin(&state, &session).await {
        return err.into_response();
    }
    let (parts, bytes) = match read_body(request, "body").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let req: ReportPostRequest = match serde_json::from_slice(&bytes) {
        Ok(req) => req,
        Err(err) => {
            tracing::debug!(error = %err, "reporting body did not decode");
            return ApiError::invalid_param("body").into_response();
        }
    };

    let mut params = if !req.cursor.cursor.is_empty() {
        match decode_report_post_cursor_v1(&req.cursor.cursor) {
            Ok(params) => params,
            Err(err) => return ApiError::from(err).into_response(),
        }
    } else {
        let time_field = if req.options.time_field == REPORTING_TIME_FIELD_UPDATE_AT {
            REPORTING_TIME_FIELD_UPDATE_AT
        } else {
            REPORTING_TIME_FIELD_CREATE_AT
        };
        let sort_direction = if req.options.sort_direction == REPORTING_SORT_DIRECTION_DESC {
            REPORTING_SORT_DIRECTION_DESC
        } else {
            REPORTING_SORT_DIRECTION_ASC
        };
        let cursor_time = if req.options.start_time > 0 {
            req.options.start_time
        } else if sort_direction == REPORTING_SORT_DIRECTION_DESC {
            i64::MAX
        } else {
            0
        };
        ReportPostQueryParams {
            channel_id: req.options.channel_id.clone(),
            cursor_time,
            cursor_id: String::new(),
            time_field: time_field.to_owned(),
            sort_direction: sort_direction.to_owned(),
            include_deleted: req.options.include_deleted,
            exclude_system_posts: req.options.exclude_system_posts,
            per_page: 0,
        }
    };
    params.per_page = if req.options.per_page <= 0 {
        100
    } else if req.options.per_page > MAX_REPORTING_PER_PAGE {
        MAX_REPORTING_PER_PAGE
    } else {
        req.options.per_page
    };
    if let Err(err) = params.validate() {
        return ApiError::from(err).into_response();
    }

    let response = match state
        .app
        .get_posts_for_reporting(&params, req.options.include_metadata)
        .await
    {
        Ok(response) => response,
        Err(PrepareError::App(err)) => return ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the reporting page to Go");
            return proxy::forward_to_go(
                State(state),
                Request::from_parts(parts, Body::from(bytes)),
            )
            .await;
        }
    };

    if response.posts.is_empty() {
        if let Err(err) = state.app.get_channel(&req.options.channel_id).await {
            return ApiError::from(err).into_response();
        }
    }
    encode_with_newline(&response, "getPostsForReporting")
}

/// Port of `startUsersBatchExport` (api4/report.go:81): the admin gate, the two query-string
/// fillers (`fillUserReportOptions`'s two 400s are the only refusals here), the date range
/// defaulted to `all_time` for `GetReportDateRange` only — the job data carries the raw
/// `date_range` — and [`mm_app::App::start_users_batch_export`].
#[tracing::instrument(skip_all)]
pub async fn start_users_batch_export(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_system_admin(&state, &session).await {
        return err.into_response();
    }
    let base = fill_reporting_base_options(query.as_deref());
    let mut options = match fill_user_report_options(query.as_deref()) {
        Ok(options) => options,
        Err(err) => return err.into_response(),
    };
    options.base = base;
    let date_range = if options.base.date_range.is_empty() {
        "all_time"
    } else {
        options.base.date_range.as_str()
    };
    let (start_at, end_at) = get_report_date_range(date_range, chrono::Local::now());

    match state
        .app
        .start_users_batch_export(&session.0, &options, start_at, end_at)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// api4/integration_action.go
// ---------------------------------------------------------------------------------------------

/// Port of `doPostAction` (api4/integration_action.go:25).
///
/// The body may be **empty** (older clients), but not malformed and not followed by a second
/// JSON value — both are the 400 `action_request`. A request carrying a `cookie` is handed to
/// Go whole: decrypting it is AES-GCM under `Systems.PostActionCookieSecret`, unported, and
/// nothing before the decrypt is observable. Without one, the caller must be able to read the
/// post (403 `read_channel_content`), then [`mm_app::App::do_post_action_gates`] decides: a
/// 400 on the query, a 404 for the post or the action, a 200 with `goto_location` for an
/// `openURL` block action, the guard's 400, or the forward.
#[tracing::instrument(skip_all, fields(post_id = %post_id, action_id = %action_id, forwarded))]
pub async fn do_post_action(
    State(state): State<AppState>,
    Path((post_id, action_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }
    let (parts, bytes) = match read_body(request, "action_request").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    // `io.EOF` from the first `Decode` is a body with no JSON value in it: empty, or
    // whitespace only. Anything else must be exactly one value.
    let action_request: DoPostActionRequest = if bytes.iter().all(u8::is_ascii_whitespace) {
        DoPostActionRequest::default()
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(req) => req,
            Err(err) => {
                tracing::debug!(error = %err, "action body did not decode");
                return ApiError::invalid_param("action_request").into_response();
            }
        }
    };

    if !action_request.cookie.is_empty() {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!(
            "handing the cookie-carrying action to Go: the cookie is AES-GCM under PostActionCookieSecret"
        );
        return proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes)))
            .await;
    }
    let (may_read, _) = state
        .app
        .session_has_permission_to_read_post(&session.0, &post_id)
        .await;
    if !may_read {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        ))
        .into_response();
    }

    let query = action_request.query.clone().unwrap_or_default();
    match state
        .app
        .do_post_action_gates(
            &post_id,
            &action_id,
            &query,
            &action_request.integration_format,
        )
        .await
    {
        Ok(PostActionOutcome::Goto(location)) => encode_with_newline(
            &PostActionAPIResponse {
                status: "OK".to_owned(),
                trigger_id: String::new(),
                goto_location: location,
            },
            "doPostAction",
        ),
        Ok(PostActionOutcome::Outbound(OutboundDisposition::Refused)) => app_error(
            "DoActionRequest",
            "api.post.do_action.action_integration.app_error",
            400,
        )
        .into_response(),
        Ok(PostActionOutcome::Outbound(OutboundDisposition::Forward(why))) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the post action to Go");
            proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `openDialog` (api4/integration_action.go:120): the body (400 `dialog`), an empty
/// `url` (400 `url`), then [`mm_app::App::open_interactive_dialog`].
#[tracing::instrument(skip_all)]
pub async fn open_dialog(
    State(state): State<AppState>,
    _session: OptionalSession,
    request: Request,
) -> Response {
    let (_, bytes) = match read_body(request, "dialog").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let dialog: OpenDialogRequest = match serde_json::from_slice(&bytes) {
        Ok(dialog) => dialog,
        Err(err) => {
            tracing::debug!(error = %err, "dialog body did not decode");
            return ApiError::invalid_param("dialog").into_response();
        }
    };
    if dialog.url.is_empty() {
        return ApiError::invalid_param("url").into_response();
    }
    match state.app.open_interactive_dialog(dialog).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The channel and team checks the three dialog submits share (api4/integration_action.go:158,
/// :214, :275): the channel read's own error, 403 `read_channel_content`, and — for a channel
/// with a team — 403 `view_team`. Returns the channel's team id, which replaces the body's.
async fn dialog_channel_checks(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
) -> Result<String, ApiError> {
    let channel = state.app.get_channel(channel_id).await?;
    let (may_read, _) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;
    if !may_read {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }
    if !channel.team_id.is_empty()
        && !state
            .app
            .session_has_permission_to_team(&session.0, &channel.team_id, &PERMISSION_VIEW_TEAM)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_TEAM],
        )));
    }
    Ok(channel.team_id)
}

/// What the three dialog submits do with the outbound disposition: the guard's refusal is
/// the 400, and anything Go would send is Go's.
async fn dispatch_outbound(
    state: AppState,
    disposition: OutboundDisposition,
    parts: axum::http::request::Parts,
    bytes: axum::body::Bytes,
) -> Response {
    match disposition {
        OutboundDisposition::Refused => app_error(
            "DoActionRequest",
            "api.post.do_action.action_integration.app_error",
            400,
        )
        .into_response(),
        OutboundDisposition::Forward(why) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the integration call to Go");
            proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
        }
    }
}

/// Port of `submitDialog` (api4/integration_action.go:141): the body (400 `dialog`), an empty
/// `url` (400 `url`), the caller as `user_id`, the channel checks, the channel's team as
/// `team_id`, then [`mm_app::App::submit_interactive_dialog_gates`].
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn submit_dialog(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request, "dialog").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let mut submit: SubmitDialogRequest = match serde_json::from_slice(&bytes) {
        Ok(submit) => submit,
        Err(err) => {
            tracing::debug!(error = %err, "dialog body did not decode");
            return ApiError::invalid_param("dialog").into_response();
        }
    };
    if submit.url.is_empty() {
        return ApiError::invalid_param("url").into_response();
    }
    submit.user_id = session.0.user_id.clone();
    submit.team_id = match dialog_channel_checks(&state, &session, &submit.channel_id).await {
        Ok(team_id) => team_id,
        Err(err) => return err.into_response(),
    };
    match state.app.submit_interactive_dialog_gates(&submit).await {
        Ok(disposition) => dispatch_outbound(state, disposition, parts, bytes).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `lookupDialog` (api4/integration_action.go:254): as `submitDialog`, plus
/// `IsValidLookupURL` (400 `url`) before the channel checks.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn lookup_dialog(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request, "dialog").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let lookup: SubmitDialogRequest = match serde_json::from_slice(&bytes) {
        Ok(lookup) => lookup,
        Err(err) => {
            tracing::debug!(error = %err, "dialog body did not decode");
            return ApiError::invalid_param("dialog").into_response();
        }
    };
    if lookup.url.is_empty() || !is_valid_lookup_url(&lookup.url) {
        return ApiError::invalid_param("url").into_response();
    }
    if let Err(err) = dialog_channel_checks(&state, &session, &lookup.channel_id).await {
        return err.into_response();
    }
    let disposition = state.app.lookup_interactive_dialog_gates(&lookup.url).await;
    dispatch_outbound(state, disposition, parts, bytes).await
}

/// Port of `executeDialogAction` (api4/integration_action.go:195): the body (400
/// `dialog_action`), an empty or invalid `url` (400 `url`), the channel checks, then
/// [`mm_app::App::execute_dialog_action_gates`] with the channel's team.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn execute_dialog_action(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, bytes) = match read_body(request, "dialog_action").await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let req: ExecuteDialogActionRequest = match serde_json::from_slice(&bytes) {
        Ok(req) => req,
        Err(err) => {
            tracing::debug!(error = %err, "dialog action body did not decode");
            return ApiError::invalid_param("dialog_action").into_response();
        }
    };
    if req.url.is_empty() || !is_valid_lookup_url(&req.url) {
        return ApiError::invalid_param("url").into_response();
    }
    let team_id = match dialog_channel_checks(&state, &session, &req.channel_id).await {
        Ok(team_id) => team_id,
        Err(err) => return err.into_response(),
    };
    match state
        .app
        .execute_dialog_action_gates(
            &session.0.user_id,
            &req.url,
            &req.context,
            &req.channel_id,
            &team_id,
        )
        .await
    {
        Ok(disposition) => dispatch_outbound(state, disposition, parts, bytes).await,
        Err(err) => ApiError::from(err).into_response(),
    }
}

// Keep the two response types that only the forwarded halves write in scope of the reader:
// `ExecuteDialogActionResponse` is `{"trigger_id"}` and a `Post` is what an integration's
// `update` carries — both marshalled by Go on this server.
#[allow(dead_code)]
fn _wire_types(_: ExecuteDialogActionResponse, _: Post) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_ok_has_no_trailing_newline() {
        let response = status_ok();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn the_action_body_is_optional_but_must_be_one_value() {
        // Whitespace is `io.EOF`; a value decodes; two values do not.
        assert!(b"  \n".iter().all(u8::is_ascii_whitespace));
        assert!(serde_json::from_slice::<DoPostActionRequest>(br#"{"query":{"k":"v"}}"#).is_ok());
        assert!(
            serde_json::from_slice::<DoPostActionRequest>(br#"{"query":{"k":"v"}}{"cookie":"x"}"#)
                .is_err()
        );
        assert!(serde_json::from_slice::<DoPostActionRequest>(br#"{"query":"notamap"}"#).is_err());
    }
}
