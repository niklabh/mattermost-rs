//! Port of the two reaction reads in `channels/api4/reaction.go`:
//! `GET /api/v4/posts/{post_id}/reactions` and `POST /api/v4/posts/ids/reactions`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_READ_CHANNEL_CONTENT, make_permission_error};
use mm_model::utils::{AppError, PAYLOAD_PARSE_ERROR, sorted_array_from_json};

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

/// Port of `getBulkReactions` (api4/reaction.go:118) — `POST /api/v4/posts/ids/reactions`.
///
/// The webapp posts the ids of every post it just loaded, so this fires once per channel switch
/// and is the busier of the two reaction reads by a wide margin.
///
/// # It validates nothing
///
/// There is no `RequirePostId`, no `is_valid_id`, and — unlike `getUsersByIds`,
/// `getRolesByNames` and every other by-ids handler in api4 — **no empty-list check**. A body of
/// `["abc"]` is a legal request that answers `{"abc":[]}` for anyone who clears the permission
/// gate. Only three bodies are refused, and two of them by accident; see below.
///
/// # Order of operations
///
/// `SortedArrayFromJSON` → the permission loop over **every** id → one store call. The gate runs
/// to completion before any read, so a list of fifty readable posts and one unreadable one is a
/// 403 with nothing leaked, and the refusal is the same 403 whichever id caused it.
///
/// # `[]` and `null` are 500s, not 400s
///
/// `SortedArrayFromJSON` returns `(nil, nil)` for a `null` body — no error — and an empty array
/// for `[]`, so both reach the app layer as zero ids. Go's store then builds `PostId IN ()`,
/// which Postgres rejects as a syntax error, and the app wraps that in a 500
/// `app.reaction.bulk_get_for_post_ids.app_error`. Measured against the running Go server, both
/// bodies answer exactly that. It is a bug on Go's side and it is on the wire, so it is ported:
/// the refusal lives in the store, where Go's does, rather than being hoisted into a tidy 400
/// here. The empty-list branch is reached **before** any permission check can fail, so a plain
/// user posting `[]` also gets the 500 rather than a 403.
///
/// A body that is not a JSON array of strings is the one honest refusal: 400
/// `api.payload.parse.error`.
///
/// # Wire format
///
/// `json.Marshal` of a `map[string][]*model.Reaction` then `w.Write` — an object with **bytewise
/// sorted keys**, `[]` (not `null`) for a post with no reactions, and **no trailing newline**.
/// See [`mm_app::App::get_bulk_reactions_for_posts`] for why the empty value differs from
/// [`get_reactions`]'s.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, asked))]
pub async fn get_bulk_reactions(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            payload_parse_error()
        })?;

    let post_ids = sorted_array_from_json(&bytes).map_err(|err| {
        tracing::debug!(error = %err, "post id body did not decode");
        payload_parse_error()
    })?;
    tracing::Span::current().record("asked", post_ids.len());

    for post_id in &post_ids {
        let (allowed, _is_member) = state
            .app
            .session_has_permission_to_read_post(&session.0, post_id)
            .await;
        if !allowed {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_READ_CHANNEL_CONTENT],
            )));
        }
    }

    let reactions = state.app.get_bulk_reactions_for_posts(&post_ids).await?;

    let body = serde_json::to_vec(&reactions).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise bulk reactions");
        ApiError::from(AppError::new(
            "getBulkReactions",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

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

/// `model.NewAppError("getBulkReactions", model.PayloadParseError, nil, "", 400)`.
fn payload_parse_error() -> ApiError {
    ApiError::from(AppError::new(
        "getBulkReactions",
        PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
}
