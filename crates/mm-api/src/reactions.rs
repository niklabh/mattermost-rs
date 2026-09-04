//! Port of `getReactions` (channels/api4/reaction.go:74), reached as
//! `GET /api/v4/posts/{post_id}/reactions`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_READ_CHANNEL_CONTENT, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;

/// Port of `getReactions` (api4/reaction.go:74).
///
/// # `null` is the empty answer, and it is load-bearing
///
/// Go writes `json.Marshal(reactions)` where `reactions` is the `[]*model.Reaction` the store
/// left **nil** for a post nothing has reacted to. A nil slice marshals to `null`, not `[]`, so
/// this route answers the four bytes `null` far more often than it answers an array — most
/// posts carry no reactions. `serde_json` renders an empty `Vec` as `[]`, which is why the
/// empty case is spelled out here rather than left to the serialiser.
///
/// # There is no 404
///
/// [`mm_app::App::get_reactions_for_post`] has no not-found branch, so a post id that names
/// nothing answers `null` with a 200 — provided the permission check above it passes. It does
/// not pass for an unknown id: `SessionHasPermissionToReadPost` cannot resolve the channel and
/// falls back to a bare system-level `read_channel_content` check, which an ordinary user
/// fails. So an unknown post is a **403**, and only a system admin sees the `null`.
///
/// # Order
///
/// `RequirePostId` → `SessionHasPermissionToReadPost` → the read. The permission check comes
/// **before** the query here, the reverse of `getPostThread` — see `MIGRATION.md`.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write`, so **no trailing newline** ([D-086]'s rule again). The 403's
/// permission id is `read_channel_content` even when the channel is open and the real refusal
/// came from the `read_public_channel` fallback: `SetPermissionError` is passed a literal, not
/// whatever the check decided.
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_reactions(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `c.RequirePostId()` (web/context.go:411). The router's `[A-Za-z0-9]+` charset has already
    // turned away the shapes gorilla would 404, leaving a segment of the wrong length to this.
    require_id(&post_id, "post_id")?;

    // The second return value is `is_member`, which Go discards here — `getReactions` builds no
    // audit record ([D-028]).
    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_read_post(&session.0, &post_id)
        .await;
    if !allowed {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    let reactions = state.app.get_reactions_for_post(&post_id).await?;

    let body = if reactions.is_empty() {
        b"null".to_vec()
    } else {
        serde_json::to_vec(&reactions).map_err(|err| {
            tracing::error!(error = %err, "failed to serialise reactions");
            ApiError::from(AppError::new(
                "getReactions",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
        })?
    };

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
