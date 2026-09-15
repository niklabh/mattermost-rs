//! Port of `searchFilesInTeam` (api4/file.go:943), `searchFilesInAllTeams` (:957) and the shared
//! `searchFiles` (:961) — `POST /api/v4/teams/{team_id}/files/search` and
//! `POST /api/v4/files/search`, the file tab of the search box.
//!
//! The handler is `searchPosts` with three differences: its own decode-failure id
//! (`api.post.search_files.invalid_body.app_error`); the result is a `FileInfoList` written by
//! `json.NewEncoder` (so with the trailing newline) and no post-list preparation runs on it; and
//! the routes are the busy-gated `APISessionRequiredDisableWhenBusy`, as the post ones are.
//!
//! # The body is decoded the way Go decodes it
//!
//! One JSON value, trailing bytes ignored, `null` a zero `SearchParameter`, an empty body the
//! decode error — [`crate::post_search`] documents the three shapes; the decoder is shared.
//!
//! # `per_page` is read, defaulted to 60, and then ignored
//!
//! As on the post routes: the database search has no paging past `page > 0`. Parsed so a
//! non-integer value is the 400 Go gives it.
//!
//! # The audit record and the metrics are not ported
//!
//! `allFilesHaveMembership` is computed and dropped ([D-028]); `IncrementFilesSearchCounter` has
//! no counterpart.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::file_search::FileSearchError;
use mm_model::permission::{PERMISSION_VIEW_TEAM, make_permission_error};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::post_search::decode_search_parameter;
use crate::proxy;

/// `w.Header().Set("Cache-Control", ...)` on the 200 (api4/file.go:1023).
const CACHE_CONTROL: &str = "no-cache, no-store, must-revalidate";

/// `perPage := 60` (api4/file.go:990).
const DEFAULT_PER_PAGE: i64 = 60;

/// What the handler decided to do, before any of it is written.
enum Outcome {
    Served(Response),
    Failed(ApiError),
    /// The Go server has to answer this one — an `in:@user` whose direct channel
    /// [`mm_app::App::get_or_create_direct_channel`] declines to open, or a cloud licence.
    Forward,
}

/// Port of `searchFilesInTeam` (api4/file.go:943) — `POST /api/v4/teams/{team_id}/files/search`.
///
/// `RequireTeamId` and the `view_team` check come **before** the body is read, so a caller
/// outside the team gets the 403 for a body that would not decode.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn search_files_in_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `APISessionRequiredDisableWhenBusy`: the busy check precedes the handler.
    if let Err(err) = crate::system::refuse_when_busy() {
        return err.into_response();
    }

    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_VIEW_TEAM]))
            .into_response();
    }

    search_files(state, &team_id, &session, request).await
}

/// Port of `searchFilesInAllTeams` (api4/file.go:957) — `POST /api/v4/files/search`. No
/// permission check of its own: the channel-membership join and
/// `FilterFilesByChannelPermissions` are the whole of the access control.
#[tracing::instrument(skip_all)]
pub async fn search_files_in_all_teams(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `APISessionRequiredDisableWhenBusy`: the busy check precedes the handler.
    if let Err(err) = crate::system::refuse_when_busy() {
        return err.into_response();
    }
    search_files(state, "", &session, request).await
}

/// The 400 `GET /api/v4/files/{file_id}` answers for `search`, which is not an id — registered
/// on the `GET` of the literal `/api/v4/files/search` so that adding the literal does not take
/// that method away from `files::get_file`; axum does not fall back across method routers
/// ([D-330]). The session comes first, as `APISessionRequired` does.
pub async fn invalid_file_id_param(_session: AuthenticatedSession) -> Response {
    ApiError::invalid_url_param("file_id").into_response()
}

/// Port of `searchFiles` (api4/file.go:961), the body shared by the two handlers.
async fn search_files(
    state: AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return invalid_body().into_response();
        }
    };

    match serve_search(&state, team_id, session, &bytes).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => {
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// `api.post.search_files.invalid_body.app_error`, 400 — the decode failure (api4/file.go:965).
fn invalid_body() -> ApiError {
    ApiError::from(AppError::new(
        "searchFiles",
        "api.post.search_files.invalid_body.app_error",
        None,
        String::new(),
        400,
    ))
}

async fn serve_search(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    bytes: &[u8],
) -> Outcome {
    let params = match decode_search_parameter(bytes) {
        Ok(params) => params,
        Err(_) => return Outcome::Failed(invalid_body()),
    };

    // `params.Terms == nil || *params.Terms == ""` — one 400 for both.
    let terms = match params.terms.as_deref() {
        Some(terms) if !terms.is_empty() => terms,
        _ => return Outcome::Failed(ApiError::invalid_param("terms")),
    };

    let time_zone_offset = params.time_zone_offset.unwrap_or(0);
    let is_or_search = params.is_or_search.unwrap_or(false);
    let page = params.page.unwrap_or(0);
    let _per_page = params.per_page.unwrap_or(DEFAULT_PER_PAGE);
    let include_deleted_channels = params.include_deleted_channels.unwrap_or(false);

    let user_id = session.0.user_id.as_str();
    let (results, _all_files_have_membership) = match state
        .app
        .search_files_in_team_for_user(
            terms,
            user_id,
            team_id,
            is_or_search,
            include_deleted_channels,
            time_zone_offset,
            page,
        )
        .await
    {
        Ok(found) => found,
        Err(FileSearchError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding the file search to Go");
            return Outcome::Forward;
        }
        Err(FileSearchError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // `json.NewEncoder(w).Encode(results)` — the encoder, so the body ends in a newline.
    let mut body = match serde_json::to_vec(&results) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise FileInfoList");
            return Outcome::Failed(ApiError::from(AppError::new(
                "searchFiles",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )));
        }
    };
    body.push(b'\n');

    Outcome::Served(
        (
            StatusCode::OK,
            [
                ("Cache-Control", CACHE_CONTROL),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}
