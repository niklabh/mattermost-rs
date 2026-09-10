//! Port of `api4/post.go`'s post writes: `updatePost` (:1060), `patchPost` (:1186) and
//! `saveIsPinnedPost` (:1353) behind both `POST /api/v4/posts/{post_id}/pin` and
//! `POST /api/v4/posts/{post_id}/unpin`.
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
use mm_model::permission::{
    PERMISSION_CREATE_POST, PERMISSION_CREATE_POST_PUBLIC, PERMISSION_EDIT_FILE_ATTACHMENT,
    PERMISSION_EDIT_OTHERS_POSTS, PERMISSION_EDIT_POST, PERMISSION_READ_CHANNEL_CONTENT,
    PERMISSION_UPLOAD_FILE, make_permission_error,
};
use mm_model::post::{POST_TYPE_CARD, Post, PostPatch};
use mm_model::utils::{AppError, is_valid_id, string_interface_to_json};

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

/// Port of `updatePost` (api4/post.go:1060) — `PUT /api/v4/posts/{post_id}`.
///
/// # Order is wire format, and this handler has nine gates in a row
///
/// `RequirePostId` → decode → `SanitizeInput` → the body's `id` must match the path's → hardened
/// mode → the post must exist (**403**, not 404) → `edit_post` on the channel → `create_post` on
/// the channel → the edit time limit → the two file permissions → the message length → ownership
/// or `edit_others_posts`. Every one of them answers before the next runs, so moving any is a
/// different status code for some request.
///
/// # A missing post is a 403 with `edit_post`, and the fetch happens **after** the body checks
///
/// `GetSinglePost`'s error is discarded for `SetPermissionError(PermissionEditPost)`. So a
/// malformed body on a nonexistent post is a 400, and a well-formed one is a 403.
///
/// # `null` file ids and `null` props mean "leave them alone"
///
/// Both are restored from the original post when the body omits them — a `PUT` that carries only
/// `id` and `message` does not strip a post's attachments. An **empty array** is not `null`: it
/// really does detach every file, which is why the two file-permission checks below can fire on a
/// body that names no ids at all.
///
/// # The time-limit gate compares four things and one of them is order-sensitive
///
/// `slices.Equal(post.FileIds, originalPost.FileIds)` is an *ordered* comparison, while
/// `checkEditFileAttachmentPermission` a few lines later uses `SliceEqualUnordered`. So reordering
/// a post's file ids is a "change" for the age limit and not a change for the permission — a
/// difference no reading of either function alone would reveal.
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn update_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("post").into_response();
        }
    };

    // `c.SetInvalidParamWithErr("post", jsonErr)` — the wrapped error only reaches
    // `detailed_error`, which the api boundary strips unless `EnableDeveloper` is on.
    let mut post: Post = match serde_json::from_slice(&bytes) {
        Ok(post) => post,
        Err(err) => {
            tracing::debug!(error = %err, "post body did not decode");
            return ApiError::invalid_param("post").into_response();
        }
    };

    // MM-67055: strips `metadata.embeds`, `delete_at` and `remote_id`. **Before** the id check.
    post.sanitize_input();

    if post.id != post_id {
        return ApiError::invalid_param("id").into_response();
    }

    match serve_update(&state, &post_id, &session, post).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the post update to Go");
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_update(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    mut post: Post,
) -> Result<Response, PrepareError> {
    post_hardened_mode_check(state, session, post.get_props())?;

    let original = state
        .app
        .get_single_post(post_id, false)
        .await
        .map_err(|_| make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]))?;

    let (may_edit, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &original.channel_id, &PERMISSION_EDIT_POST)
        .await;
    if !may_edit {
        return Err(PrepareError::App(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_POST],
        )));
    }

    // "Users who can't create posts in a channel shouldn't be able to edit them either."
    user_create_post_permission_check(state, session, &original.channel_id).await?;

    // A nil list or map means "no change", so the original's is restored before anything measures
    // a difference.
    if post.file_ids.is_none() {
        post.file_ids = original.file_ids.clone();
    }
    if post.props.is_none() {
        post.set_props(original.get_props().cloned());
    }

    let limit = state.app.config().post_edit_time_limit;
    if post_edit_time_limit_expired(limit, &original)
        && (post.message != original.message
            // `slices.Equal` — **ordered**, unlike the permission check below.
            || post.file_ids.as_deref().unwrap_or_default()
                != original.file_ids.as_deref().unwrap_or_default()
            || string_interface_to_json(post.get_props())
                != string_interface_to_json(original.get_props())
            || post.is_pinned != original.is_pinned)
    {
        return Err(time_limit_error("UpdatePost", limit));
    }

    let new_file_ids = post.file_ids.as_deref().unwrap_or_default();
    check_upload_file_permission_for_new_files(state, session, new_file_ids, &original).await?;
    check_edit_file_attachment_permission(state, session, new_file_ids, &original).await?;

    reject_oversized_message(state, "Api4.updatePost", &post.message).await?;

    if original.post_type == POST_TYPE_CARD {
        // The collaborative-card branch skips the ownership check, and its flag is not in the
        // configuration document — the app layer refuses the same shape.
        return Err(PrepareError::Unreproducible(
            "a card post's ownership check turns on FeatureFlags.IntegratedBoards",
        ));
    }
    if session.0.user_id != original.user_id {
        let (may_edit_others, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &original.channel_id,
                &PERMISSION_EDIT_OTHERS_POSTS,
            )
            .await;
        if !may_edit_others {
            return Err(PrepareError::App(make_permission_error(
                &session.0,
                &[&PERMISSION_EDIT_OTHERS_POSTS],
            )));
        }
    }

    // Go reassigns `post.Id = c.Params.PostId` here, after the checks. It is already equal — the
    // gate above refuses anything else — so the assignment is Go's belt and braces.
    post.id = post_id.to_owned();

    let (updated, _is_member_for_previews) = state.app.update_post(&post, &session.0).await?;
    encoded_post(updated)
}

/// Port of `patchPost` (api4/post.go:1186) — `PUT /api/v4/posts/{post_id}/patch`.
///
/// # It reads the post three times and the *second* read decides the permission
///
/// `postPatchChecks` fetches it to pick between `edit_post` and `edit_others_posts`, this handler
/// fetches it again for the file checks, and `App.PatchPost` fetches it a third time to patch. All
/// three are reproduced; the redundancy is Go's.
///
/// # The permission depends on authorship, unlike `updatePost`
///
/// `updatePost` checks `edit_post` for everyone and *then* `edit_others_posts` for a non-author.
/// `patchPost` picks one of the two up front — so a caller holding `edit_others_posts` but not
/// `edit_post` is refused by `PUT /posts/{id}` and served by `PUT /posts/{id}/patch`.
///
/// # An empty patch escapes the age limit
///
/// `postEditTimeLimitExpired(...) && !patch.IsEmpty()` — a patch with every field `null` on a post
/// older than the limit is a **200** that changes nothing but still writes a row and an
/// edit-history entry. `IsEmpty` tests the five patch fields for nil, so `{}` and
/// `{"message":null}` are both empty and `{"message":""}` is not.
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn patch_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("post").into_response();
        }
    };

    let patch: PostPatch = match serde_json::from_slice(&bytes) {
        Ok(patch) => patch,
        Err(err) => {
            tracing::debug!(error = %err, "patch body did not decode");
            return ApiError::invalid_param("post").into_response();
        }
    };

    match serve_patch(&state, &post_id, &session, &patch).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the post patch to Go");
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_patch(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    patch: &PostPatch,
) -> Result<Response, PrepareError> {
    // Only when the patch *carries* props — a patch that leaves them alone is never checked, so a
    // post keeps whatever reserved props it already had.
    if let Some(props) = patch.props.as_ref() {
        post_hardened_mode_check(state, session, Some(props))?;
    }

    post_patch_checks(state, post_id, session, patch).await?;

    if let Some(message) = patch.message.as_ref() {
        reject_oversized_message(state, "Api4.patchPost", message).await?;
    }

    let original = state
        .app
        .get_single_post(post_id, false)
        .await
        .map_err(|_| make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]))?;

    if let Some(file_ids) = patch.file_ids.as_ref() {
        check_upload_file_permission_for_new_files(state, session, file_ids, &original).await?;
        check_edit_file_attachment_permission(state, session, file_ids, &original).await?;
    }

    let (patched, _is_member_for_previews) =
        state.app.patch_post(post_id, patch, &session.0).await?;
    encoded_post(patched)
}

/// Port of `postPatchChecks` (api4/post.go:1255). Returns nothing: Go's `isMember` return feeds
/// the audit record only ([D-028]).
async fn post_patch_checks(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    patch: &PostPatch,
) -> Result<(), PrepareError> {
    let original = state
        .app
        .get_single_post(post_id, false)
        .await
        .map_err(|_| make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]))?;

    if original.post_type == POST_TYPE_CARD {
        return Err(PrepareError::Unreproducible(
            "a card post's permission choice turns on FeatureFlags.IntegratedBoards",
        ));
    }
    let permission = if session.0.user_id == original.user_id {
        &PERMISSION_EDIT_POST
    } else {
        &PERMISSION_EDIT_OTHERS_POSTS
    };

    let (granted, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &original.channel_id, permission)
        .await;
    if !granted {
        return Err(PrepareError::App(make_permission_error(
            &session.0,
            &[permission],
        )));
    }

    user_create_post_permission_check(state, session, &original.channel_id).await?;

    let limit = state.app.config().post_edit_time_limit;
    if post_edit_time_limit_expired(limit, &original) && !patch.is_empty() {
        return Err(time_limit_error("patchPost", limit));
    }

    Ok(())
}

/// Port of `userCreatePostPermissionCheckWithContext` (api4/post_utils.go:12).
///
/// Two arms, and Go's own comment on the second says "temporary permission check method until
/// advanced permissions, please do not copy". Its `GetChannel` error is **swallowed**, so a channel
/// that cannot be read simply fails the fallback.
async fn user_create_post_permission_check(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
) -> Result<(), PrepareError> {
    let (granted, _) = state
        .app
        .session_has_permission_to_channel(&session.0, channel_id, &PERMISSION_CREATE_POST)
        .await;
    if granted {
        return Ok(());
    }

    if let Ok(channel) = state.app.get_channel(channel_id).await {
        if channel.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN
            && state
                .app
                .session_has_permission_to_team(
                    &session.0,
                    &channel.team_id,
                    &PERMISSION_CREATE_POST_PUBLIC,
                )
                .await
        {
            return Ok(());
        }
    }

    Err(PrepareError::App(make_permission_error(
        &session.0,
        &[&PERMISSION_CREATE_POST],
    )))
}

/// Port of `postHardenedModeCheck` (app/post_permission_utils.go:90).
///
/// Off — the default — the whole check is a no-op, which is why being wrong about
/// `ExperimentalEnableHardenedMode` would be silent: every post would be accepted. `isIntegration`
/// here is `Session.IsIntegration()`, the **wide** one that includes personal access tokens, not
/// the narrower `isIntegrationPostAuthor` the notification path uses.
fn post_hardened_mode_check(
    state: &AppState,
    session: &AuthenticatedSession,
    props: Option<&mm_model::utils::StringInterface>,
) -> Result<(), PrepareError> {
    if !state.app.config().experimental_enable_hardened_mode {
        return Ok(());
    }
    let reserved = mm_model::post::contains_integrations_reserved_props(props);
    if reserved.is_empty() || session.0.is_integration() {
        return Ok(());
    }
    // Go's `Where` is empty at construction and overwritten by the caller
    // (`appErr.Where = where`), which is why the id is the only part a client sees.
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert("Name".to_owned(), serde_json::json!("props"));
    Err(PrepareError::App(AppError::boxed(
        "UpdatePost",
        "api.context.invalid_body_param.app_error",
        Some(params),
        format!("Cannot use props reserved for integrations. props: {reserved:?}"),
        400,
    )))
}

/// Port of `checkUploadFilePermissionForNewFiles` (api4/post_utils.go:63).
///
/// Only fires for an id the post does not already carry, so a caller who lost `upload_file` can
/// still edit the message of a post with attachments.
async fn check_upload_file_permission_for_new_files(
    state: &AppState,
    session: &AuthenticatedSession,
    new_file_ids: &[String],
    original: &Post,
) -> Result<(), PrepareError> {
    if new_file_ids.is_empty() {
        return Ok(());
    }
    let existing = original.file_ids.as_deref().unwrap_or_default();
    if !new_file_ids.iter().any(|id| !existing.contains(id)) {
        return Ok(());
    }
    let (granted, _) = state
        .app
        .session_has_permission_to_channel(
            &session.0,
            &original.channel_id,
            &PERMISSION_UPLOAD_FILE,
        )
        .await;
    if granted {
        return Ok(());
    }
    Err(PrepareError::App(make_permission_error(
        &session.0,
        &[&PERMISSION_UPLOAD_FILE],
    )))
}

/// Port of `checkEditFileAttachmentPermission` (api4/post_utils.go:91).
///
/// `utils.SliceEqualUnordered` — a **multiset** comparison, so reordering a post's file ids is not
/// a change here even though the age-limit gate in `updatePost` counts it as one.
async fn check_edit_file_attachment_permission(
    state: &AppState,
    session: &AuthenticatedSession,
    new_file_ids: &[String],
    original: &Post,
) -> Result<(), PrepareError> {
    if slice_equal_unordered(
        new_file_ids,
        original.file_ids.as_deref().unwrap_or_default(),
    ) {
        return Ok(());
    }
    let (granted, _) = state
        .app
        .session_has_permission_to_channel(
            &session.0,
            &original.channel_id,
            &PERMISSION_EDIT_FILE_ATTACHMENT,
        )
        .await;
    if granted {
        return Ok(());
    }
    Err(PrepareError::App(make_permission_error(
        &session.0,
        &[&PERMISSION_EDIT_FILE_ATTACHMENT],
    )))
}

/// Port of `utils.SliceEqualUnordered` (channels/utils/utils.go:283) — equal lengths and equal
/// multisets, so a repeated id has to be repeated the same number of times on both sides.
fn slice_equal_unordered(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut counts: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for id in a {
        *counts.entry(id.as_str()).or_default() += 1;
    }
    for id in b {
        let entry = counts.entry(id.as_str()).or_default();
        *entry -= 1;
        if *entry < 0 {
            return false;
        }
    }
    true
}

/// Port of `rejectOversizedMessage` (api4/post.go:64).
///
/// It measures **runes**, and it runs before the markdown processing so an oversized message is
/// rejected before anything pays for it. The same limit is checked again inside
/// `Post::is_valid`; this one exists to answer earlier and with the handler's own `where`.
async fn reject_oversized_message(
    state: &AppState,
    where_: &'static str,
    message: &str,
) -> Result<(), PrepareError> {
    let max_post_size = state.app.max_post_size().await?;
    let length = message.chars().count();
    if length <= max_post_size {
        return Ok(());
    }
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert("Length".to_owned(), serde_json::json!(length));
    params.insert("MaxLength".to_owned(), serde_json::json!(max_post_size));
    Err(PrepareError::App(AppError::boxed(
        where_,
        "model.post.is_valid.message_length.app_error",
        Some(params),
        String::new(),
        400,
    )))
}

/// `api.post.update_post.permissions_time_limit.app_error`, which three routes raise with the same
/// `timeLimit` parameter and three different `where`s.
fn time_limit_error(where_: &'static str, limit: i64) -> PrepareError {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert("timeLimit".to_owned(), serde_json::json!(limit));
    PrepareError::App(AppError::boxed(
        where_,
        "api.post.update_post.permissions_time_limit.app_error",
        Some(params),
        String::new(),
        400,
    ))
}

/// `rpost.EncodeJSON(w)` — strips the private action integrations **in place** and appends the
/// newline `json.Encoder` writes and `json.Marshal` does not.
fn encoded_post(mut post: Post) -> Result<Response, PrepareError> {
    let mut body = Vec::new();
    if let Err(err) = post.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise Post");
        return Err(PrepareError::App(AppError::boxed(
            "updatePost",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }
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
