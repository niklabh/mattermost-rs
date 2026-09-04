//! Port of `getFileInfo` (channels/api4/file.go:841), reached as
//! `GET /api/v4/files/{file_id}/info`.
//!
//! # The only `/files/` route that returns JSON
//!
//! Its five siblings under `BaseRoutes.File` — `""`, `/thumbnail`, `/preview`, `/link` and the
//! unauthenticated `/public` — all serve file *bytes* or a signed URL out of the file backend,
//! which this port does not have. They are unregistered and fall to `Router::fallback`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_model::file_info::BOOKMARK_FILE_OWNER;
use mm_model::permission::{PERMISSION_READ_CHANNEL_CONTENT, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::posts::FILE_CACHE_CONTROL;
use crate::proxy;

/// Port of `getFileInfo` (api4/file.go:841).
///
/// # The permission block is three branches, not two, and the middle one is the surprise
///
/// ```text
/// if info.CreatorId == model.BookmarkFileOwner {
///     if !perm { 403 }
/// } else if info.CreatorId != session.UserId && !perm {
///     403
/// }
/// ```
///
/// A file **you uploaded** is readable without any channel permission at all — `CreatorId ==
/// session.UserId` short-circuits the second branch — but a *bookmark* file is not: the literal
/// owner `"bookmark"` can never equal a real user id, so the first branch exists solely to
/// remove that escape hatch. Collapsing the two into a single `!perm` test would deny a user
/// their own file after they left the channel; collapsing them the other way would hand every
/// user every channel bookmark.
///
/// `perm` comes from `SessionHasPermissionToReadChannel`, and the 403 reports
/// `read_channel_content` in both branches.
///
/// # The channel lookup runs before the permission check and can 404 on its own
///
/// `info.ChannelId` is `COALESCE(FileInfo.ChannelId, '')` in the store, and the column really is
/// nullable — it was added after `FileInfo` existed. The upload path fills it in
/// (`t.fileinfo.ChannelId = t.ChannelId`, app/file.go:774), so every row the current server
/// writes has one; a row from before that migration does not, arrives here as the empty string,
/// and `GetChannel("")` finds nothing. That is a **404** `app.channel.get.existing.app_error`
/// and not a 403 — so such a file is unreadable even by the user who uploaded it, because the
/// `CreatorId == session.UserId` escape hatch is two lines too late to matter.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(info)` — **trailing newline** ([D-086]) — plus
/// `Cache-Control: max-age=2592000, private`. There is no etag on this route at all, so
/// `If-None-Match` is ignored and every request is a 200.
///
/// # `First-Inaccessible-File-Time` is not reproduced
///
/// `setInaccessibleFileHeader` (api4/file.go:1032) sets it when `GetFileInfo` fails with
/// `app.file.cloud.get.app_error`, which needs a licence carrying a `Files` limit. Unreachable
/// here — the same treatment as `First-Inaccessible-Post-Time` in [`crate::posts`].
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_file_info(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve(&state, &file_id, &session).await {
        Ok(Some(response)) => response,
        // The mini-preview repair — see `mm_app::App::get_file_info`.
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn serve(
    state: &AppState,
    file_id: &str,
    session: &AuthenticatedSession,
) -> Result<Option<Response>, ApiError> {
    // `c.RequireFileId()` (web/context.go:455). The router's `[A-Za-z0-9]+` charset has already
    // turned away everything gorilla would 404, leaving the length check to this.
    require_id(file_id, "file_id")?;

    let info = match state.app.get_file_info(file_id).await {
        Ok(info) => info,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, file_id, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(err)),
    };

    let channel = state.app.get_channel(&info.channel_id).await?;

    let (perm, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;

    let denied = if info.creator_id == BOOKMARK_FILE_OWNER {
        !perm
    } else {
        info.creator_id != session.0.user_id && !perm
    };
    if denied {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    // Unconditionally true on this deployment; see `mm_app::file::has_permission_to_file_action`.
    if !mm_app::file::has_permission_to_file_action() {
        return Err(ApiError::from(*mm_app::file::abac_denied("getFileInfo")));
    }

    let mut body = serde_json::to_vec(&info).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise FileInfo");
        ApiError::from(AppError::new(
            "getFileInfo",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok(Some(
        (
            StatusCode::OK,
            [
                ("Cache-Control", FILE_CACHE_CONTROL),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    ))
}
