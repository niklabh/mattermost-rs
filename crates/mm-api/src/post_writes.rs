//! Port of `api4/post.go`'s post writes: `saveIsPinnedPost` (:1353) behind both
//! `POST /api/v4/posts/{post_id}/pin` and `POST /api/v4/posts/{post_id}/unpin`.
//!
//! Kept out of [`crate::posts`], which is 1,800 lines of read path.
//!
//! # Pinning a post rewrites it
//!
//! There is no `IsPinned` column update in Go. `saveIsPinnedPost` builds a `PostPatch` carrying
//! only `IsPinned` and goes through `PatchPost` → `UpdatePost` → `SqlPostStore.Update`, which
//! bumps `UpdateAt`, moves the channel's `LastPostAt` and **writes an edit-history row**. So a pin
//! is observable through `GET /posts/{id}/edit_history` and through the channel's sidebar order,
//! not only through `is_pinned`.
//!
//! # The two routes are not symmetric with each other
//!
//! They share one function, but the *state* they act on is not symmetric: the no-op short circuit
//! fires when `post.IsPinned == isPinned`, so pinning a pinned post is a **200 with no write and
//! no websocket event**, while pinning an unpinned one is a full edit. The comment in Go says why
//! the short circuit is before the time-limit check — "allow no-op requests regardless of age" —
//! and that ordering is the whole difference between a 200 and a 400 for an old post.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_app::post_write::post_edit_time_limit_expired;
use mm_model::permission::{PERMISSION_READ_CHANNEL_CONTENT, make_permission_error};
use mm_model::post::PostPatch;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `pinPost` (api4/post.go:1419).
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn pin_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    save_is_pinned_post(state, post_id, session, request, true).await
}

/// Port of `unpinPost` (api4/post.go:1423).
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn unpin_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    save_is_pinned_post(state, post_id, session, request, false).await
}

/// Port of `saveIsPinnedPost` (api4/post.go:1353).
///
/// # A post that is not there is a **403**, not a 404
///
/// `GetSinglePost`'s error is discarded and replaced with
/// `SetPermissionError(PermissionReadChannelContent)`. So a well-formed id for a post that never
/// existed answers 403, and so does one for a post in a channel the caller cannot read — which is
/// the point: the two are indistinguishable to a caller probing for post ids.
///
/// # The permission is a *read* permission
///
/// Pinning needs `read_channel_content` and nothing else. A guest who can read a channel can pin
/// anybody's post in it; there is no `edit_post` check on this route at all.
///
/// # The body is `{"status":"OK"}` and never the post
///
/// `ReturnStatusOK` on every success — the patched post is computed, audited and thrown away. A
/// client that wants the new `is_pinned` has to re-read the post or wait for the `post_edited`
/// event.
async fn save_is_pinned_post(
    state: AppState,
    post_id: String,
    session: AuthenticatedSession,
    request: Request,
    is_pinned: bool,
) -> Response {
    // `c.RequirePostId()` (web/context.go:411). The router's charset middleware has already
    // handled the shapes gorilla 404s, so what is left is a segment of the wrong length.
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }

    match serve(&state, &post_id, &session, is_pinned).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the pin write to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    is_pinned: bool,
) -> Result<Response, PrepareError> {
    let post = state
        .app
        .get_single_post(post_id, false)
        .await
        // Go throws the app error away. Every failure — not found, or a database fault — becomes
        // the same 403.
        .map_err(|_| make_permission_error(&session.0, &[&PERMISSION_READ_CHANNEL_CONTENT]))?;

    let channel = state.app.get_channel(&post.channel_id).await?;

    let (may_read, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;
    if !may_read {
        return Err(PrepareError::App(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    // **Before** the time-limit check, so pinning an already-pinned post never 400s on age.
    if post.is_pinned == is_pinned {
        return Ok(status_ok());
    }

    let limit = state.app.config().post_edit_time_limit;
    if post_edit_time_limit_expired(limit, &post) {
        return Err(PrepareError::App(AppError::boxed(
            "saveIsPinnedPost",
            "api.post.update_post.permissions_time_limit.app_error",
            Some(std::collections::HashMap::from([(
                "timeLimit".to_owned(),
                serde_json::json!(limit),
            )])),
            String::new(),
            400,
        )));
    }

    let patch = PostPatch {
        is_pinned: Some(is_pinned),
        ..PostPatch::default()
    };

    // The patched post and `isMemberForPreviews` both feed the audit record only ([D-028]).
    let (_patched, _is_member_for_previews) =
        state.app.patch_post(post_id, &patch, &session.0).await?;

    Ok(status_ok())
}

/// `ReturnStatusOK` (web/context.go) — written with `w.Write`, so **no trailing newline**, unlike
/// every route that goes through `json.Encoder`.
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
