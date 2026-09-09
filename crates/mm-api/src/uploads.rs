//! The two upload-session reads: `getUpload` (api4/upload.go:100) and `getUploadsForUser`
//! (api4/user.go:3710).
//!
//! Both answer with `UploadSession` rows and neither touches the file backend — an upload session
//! is the *bookkeeping* for a resumable upload, and the bytes it describes live under a `.tmp`
//! path that only the write half ever opens. They are here because they are the read side of the
//! upload flow, and because their two authorisation rules disagree in a way worth stating.
//!
//! # One route lets an admin look and the other does not
//!
//! | | `getUpload` | `getUploadsForUser` |
//! |---|---|---|
//! | rule | `us.UserId == session.UserId` **or** `manage_system` | `user_id == session.UserId`, full stop |
//! | error id | `api.upload.get_upload.forbidden.app_error` | `api.user.get_uploads_for_user.forbidden.app_error` |
//! | body | `Encode` — **trailing newline** | `Marshal` + `Write` — **none** |
//!
//! So a system administrator can read any single upload session but cannot list another user's.
//!
//! `me` **is** accepted, and that is not obvious from `getUploadsForUser`: the substitution
//! happens inside `RequireUserId` (web/context.go:301), two lines before the `IsValidId` it looks
//! like it is only there for, so the comparison below is against the *resolved* id. A port that
//! validated the raw segment answers 400 where Go answers 200 — measured, on this very route,
//! by `tests/parity/exports_and_uploads.rs`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::PERMISSION_MANAGE_SYSTEM;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{require_id, resolve_me};
use crate::error::ApiError;

/// Port of `getUpload` (api4/upload.go:100), reached as `GET /api/v4/uploads/{upload_id}`.
///
/// The permission check runs **after** the row is read, so a caller who may not see it still
/// learns whether it exists — a 404 for a missing id and a 403 for someone else's.
#[tracing::instrument(skip_all, fields(upload_id = %upload_id))]
pub async fn get_upload(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    match serve_upload(&state, &upload_id, &session).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_upload(
    state: &AppState,
    upload_id: &str,
    session: &AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(upload_id, "upload_id")?;

    let upload = state.app.get_upload_session(upload_id).await?;

    // `c.IsSystemAdmin()` is `SessionHasPermissionTo(manage_system)`, evaluated only when the
    // session is not the owner — Go's `&&` short-circuits, so an owner's read never asks the
    // permission layer anything.
    if upload.user_id != session.0.user_id
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        // **Not** `SetPermissionError`: a hand-built `AppError` with its own id, so this 403
        // carries no `permissions` detail and does not name `manage_system`.
        return Err(ApiError::from(*AppError::boxed(
            "getUpload",
            "api.upload.get_upload.forbidden.app_error",
            None,
            String::new(),
            403,
        )));
    }

    let mut body = serde_json::to_vec(&upload).map_err(|err| marshal_error(&err, "getUpload"))?;
    // `json.NewEncoder(w).Encode` — trailing newline ([D-086]).
    body.push(b'\n');

    Ok(json_response(body))
}

/// Port of `getUploadsForUser` (api4/user.go:3710), reached as
/// `GET /api/v4/users/{user_id}/uploads`.
///
/// # `me` resolves first, so `/users/me/uploads` is the caller's own list
///
/// `RequireUserId` substitutes the session's user id for the literal `me` **before** it validates
/// (web/context.go:301), so by the time `c.Params.UserId != session.UserId` runs the two are
/// equal. Nothing in this handler says so, which is exactly why the first version of this port
/// answered 400 to a request Go answers 200.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn get_uploads_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    match serve_uploads_for_user(&state, &user_id, &session).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_uploads_for_user(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = resolve_me(user_id, session);
    require_id(user_id, "user_id")?;

    if user_id != session.0.user_id {
        return Err(ApiError::from(*AppError::boxed(
            "getUploadsForUser",
            "api.user.get_uploads_for_user.forbidden.app_error",
            None,
            String::new(),
            403,
        )));
    }

    let uploads = state.app.get_upload_sessions_for_user(user_id).await?;

    // `json.Marshal` then `w.Write` — **no** trailing newline, unlike `getUpload` one module
    // away. The store's empty-slice initialiser means this is `[]` and never `null`.
    let body =
        serde_json::to_vec(&uploads).map_err(|err| marshal_error(&err, "getUploadsForUser"))?;

    Ok(json_response(body))
}

fn json_response(body: Vec<u8>) -> Response {
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

fn marshal_error(err: &serde_json::Error, where_: &'static str) -> ApiError {
    tracing::error!(error = %err, "failed to serialise the upload session");
    ApiError::from(*AppError::boxed(
        where_,
        "api.marshal_error",
        None,
        String::new(),
        500,
    ))
}
