//! Port of `api4/post.go`'s post writes: `deletePost` (:745), `updatePost` (:1060),
//! `patchPost` (:1186) and `saveIsPinnedPost` (:1353) behind both
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
use mm_app::license::LicenseState;
use mm_app::post::PrepareError;
use mm_app::post_create::CreatePostFlags;
use mm_app::post_write::post_edit_time_limit_expired;
use mm_model::permission::{
    PERMISSION_CREATE_POST, PERMISSION_CREATE_POST_EPHEMERAL, PERMISSION_CREATE_POST_PUBLIC,
    PERMISSION_DELETE_OTHERS_POSTS, PERMISSION_DELETE_POST, PERMISSION_EDIT_FILE_ATTACHMENT,
    PERMISSION_EDIT_OTHERS_POSTS, PERMISSION_EDIT_POST, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_UPLOAD_FILE, make_permission_error,
};
use mm_model::post::{POST_TYPE_CARD, Post, PostEphemeral, PostPatch};
use mm_model::utils::{AppError, is_valid_id, parse_go_bool, string_interface_to_json};

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

/// Port of `deletePost` (api4/post.go:745) — `DELETE /api/v4/posts/{post_id}`.
///
/// # `?permanent=true` is forwarded whole
///
/// It selects `PermanentDeletePost`, a hard delete with its own cascade across seven tables, and
/// it is gated on `ServiceSettings.EnableAPIPostDeletion` (**501** when off) and on
/// `manage_system` (403). None of that is ported, and the forward happens before any of it, so Go
/// answers the 501 and the 403 as well as the delete. `strconv.ParseBool` with the error
/// discarded, so `?permanent=yes` is `false` and lands here rather than upstream.
///
/// # The permission split is on authorship, and the refusal names different permissions
///
/// Deleting your own post needs `delete_post`; deleting anybody else's needs
/// `delete_others_posts`. The 403 names whichever was missing, and clients branch on that id.
///
/// # The body is `{"status":"OK"}`
///
/// The deleted post is returned by the app layer and dropped by the handler.
#[tracing::instrument(skip_all, fields(post_id = %post_id, forwarded))]
pub async fn delete_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&post_id) {
        return ApiError::invalid_url_param("post_id").into_response();
    }

    // `c.Params.Permanent` is `strconv.ParseBool(query.Get("permanent"))` with the error dropped
    // (web/params.go:232) — so `?permanent=yes` is `false`, not a 400.
    let permanent = crate::channels::query_flag_is_true(request.uri().query(), "permanent");
    if permanent {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!("handing a permanent delete to Go");
        return proxy::forward_to_go(State(state), request).await;
    }

    match serve_delete(&state, &post_id, &session).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the post delete to Go");
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_delete(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
) -> Result<Response, PrepareError> {
    // Unlike the pin and edit routes, this one lets `GetSinglePost`'s error through: a post that
    // does not exist is a **404 `app.post.get.app_error`** here and a 403 there.
    let post = state.app.get_single_post(post_id, false).await?;

    if post.post_type == POST_TYPE_CARD {
        // The card arm gives *any* holder of `delete_post` the right to delete somebody else's
        // card, and it turns on `FeatureFlags.IntegratedBoards`, which is not in the configuration
        // document either server persists.
        return Err(PrepareError::Unreproducible(
            "a card post's delete permission turns on FeatureFlags.IntegratedBoards",
        ));
    }

    let permission = if session.0.user_id == post.user_id {
        &PERMISSION_DELETE_POST
    } else {
        &PERMISSION_DELETE_OTHERS_POSTS
    };
    let (granted, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &post.channel_id, permission)
        .await;
    if !granted {
        return Err(PrepareError::App(make_permission_error(
            &session.0,
            &[permission],
        )));
    }

    state
        .app
        .delete_post(post_id, &session.0.user_id)
        .await
        .map(|_deleted| status_ok())
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

// ===========================================================================================
// createPost / createEphemeralPost
// ===========================================================================================

/// Port of `createPost` (api4/post.go:111) — `POST /api/v4/posts`.
///
/// # Nine gates before the write, and the order is the wire format
///
/// decode → `SanitizeInput` → the session's user id is stamped over whatever the body claimed →
/// `CreateAt` is zeroed unless the caller holds `manage_system` → `createPostChecks`' six checks
/// in their own order → the message-length check → `?silent` → the app layer. Every one of them
/// answers before the next runs, and three of them are easy to reorder wrongly: the `CreateAt`
/// gate reads a **system** permission while the next check reads a channel one, `?silent=bogus`
/// is a 400 that a permission failure gets in front of, and `rejectOversizedMessage` runs
/// *after* the permission checks so an oversized message in a channel you cannot post to is a
/// 403.
///
/// # `?set_online` swallows its parse error and `?silent` does not
///
/// `set_online=bogus` logs a warning and stays **true**; `silent=bogus` is
/// `api.context.invalid_param.app_error` at 400. Two `strconv.ParseBool` calls a few lines apart,
/// with opposite error handling — a port that shared one helper between them would answer 400 for
/// both or 200 for both.
///
/// # The response is **201**, and the post has been through `PreparePostForClient` already
///
/// `w.WriteHeader(http.StatusCreated)` before the encode, so this is the one post route that is
/// not a 200. The burn-on-read re-read that follows in Go needs that post type, which this port
/// forwards.
///
/// # What is forwarded, and why the forward is always before the row
///
/// `mm_app::post_create` lists the shapes; what matters here is that every one of them is decided
/// inside `App::create_post_as_user` before `Post().Save` and before the pending-post id is
/// claimed. A forward that happened after either would leave Go to write a second row.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn create_post(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("post").into_response();
        }
    };

    // `c.SetInvalidParamWithErr("post", jsonErr)`.
    let mut post: Post = match serde_json::from_slice(&bytes) {
        Ok(post) => post,
        Err(err) => {
            tracing::debug!(error = %err, "post body did not decode");
            return ApiError::invalid_param("post").into_response();
        }
    };

    // MM-67055: strips `metadata.embeds`, `delete_at` and `remote_id` — the last of which is what
    // makes `SanitizeProps` treat the post as *not* federated a moment later, so both
    // `force_notification` and `silent_notification` are stripped from a REST create.
    post.sanitize_input();
    post.user_id.clone_from(&session.0.user_id);

    let query = parts.uri.query().map(str::to_owned);

    match serve_create(&state, &session, post, query.as_deref()).await {
        Ok(response) => response,
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the post create to Go");
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_create(
    state: &AppState,
    session: &AuthenticatedSession,
    mut post: Post,
    query: Option<&str>,
) -> Result<Response, PrepareError> {
    // "if post.CreateAt != 0 && !c.App.SessionHasPermissionTo(session, PermissionManageSystem)".
    // A **system** permission, not a channel one, and the failure is silent: the timestamp is
    // dropped and the post is still created. Reversing the test would let any client backdate a
    // post.
    if post.create_at != 0
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        post.create_at = 0;
    }

    create_post_checks("Api4.createPost", state, session, &post).await?;
    reject_oversized_message(state, "Api4.createPost", &post.message).await?;

    // `strconv.ParseBool` with the error **warned and discarded** — the default is true and an
    // unparseable value keeps it.
    let set_online = match query_value(query, "set_online") {
        Some(raw) if !raw.is_empty() => parse_go_bool(&raw).unwrap_or_else(|| {
            tracing::warn!(
                raw,
                "Failed to parse set_online URL query parameter from createPost request"
            );
            true
        }),
        _ => true,
    };

    // The sibling that does *not* swallow: `c.SetInvalidParam("silent")`.
    let silent = match query_value(query, "silent") {
        Some(raw) if !raw.is_empty() => match parse_go_bool(&raw) {
            Some(value) => value,
            None => return Err(PrepareError::App(invalid_param_error("silent"))),
        },
        _ => false,
    };

    // `PostWithProxyRemovedFromImageURLs` is the identity when the proxy is off, and
    // `App::create_post_as_user` refuses the post when it is on — reached through
    // `prepare_post_for_client`, which runs before the response and after nothing that writes.

    let created = state
        .app
        .create_post_as_user(
            post,
            &session.0,
            CreatePostFlags {
                set_online,
                silent_notification: silent,
            },
        )
        .await?;

    if set_online {
        state.app.set_status_online(&session.0.user_id, false).await;
    }

    // `c.App.Srv().Platform().UpdateLastActivityAtIfNeeded(*c.AppContext.Session())` — throttled,
    // so most requests write nothing. `ExtendSessionExpiryIfNeeded` needs
    // `ExtendSessionLengthWithActivity`, which is off by default and is not modelled.
    state
        .app
        .update_last_activity_at_if_needed(&session.0)
        .await;

    created_post(created)
}

/// Port of `createEphemeralPost` (api4/post.go:215) — `POST /api/v4/posts/ephemeral`.
///
/// # It writes no row, and that is the whole reason it is served
///
/// `SendEphemeralPost` builds a post in memory, prepares it, pushes it down one user's websocket
/// and returns it. There is no `Post().Save`, no channel counter and no thread row — so the only
/// way this route can be wrong is by answering with a different *shape*, never by leaving a row
/// Go would not have written.
///
/// # Three 400s before the permission check, and the permission is a system one
///
/// A body that does not decode is `body`, an empty `user_id` is `user_id`, and a null `post` is
/// `post` — then `create_post_ephemeral`, checked on the session's **system** roles rather than
/// on the channel. So a system admin can push an ephemeral post into a channel they cannot read,
/// and a channel admin cannot push one into their own channel.
///
/// # `create_at` is overwritten, not defaulted
///
/// `ephRequest.Post.CreateAt = model.GetMillis()` — unconditionally, before the permission check.
/// Unlike `createPost` there is no `manage_system` escape hatch and no zero test, so a client's
/// timestamp is always discarded.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn create_ephemeral_post(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("body").into_response();
        }
    };

    // `c.SetInvalidParamWithErr("body", jsonErr)` — the parameter name is `body` here and `post`
    // on the sibling route.
    let ephemeral: PostEphemeral = match serde_json::from_slice(&bytes) {
        Ok(ephemeral) => ephemeral,
        Err(err) => {
            tracing::debug!(error = %err, "ephemeral post body did not decode");
            return ApiError::invalid_param("body").into_response();
        }
    };

    if ephemeral.user_id.is_empty() {
        return ApiError::invalid_param("user_id").into_response();
    }
    let Some(mut post) = ephemeral.post else {
        return ApiError::invalid_param("post").into_response();
    };

    post.user_id.clone_from(&session.0.user_id);
    post.create_at = mm_model::utils::get_millis();

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_CREATE_POST_EPHEMERAL)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_POST_EPHEMERAL],
        ))
        .into_response();
    }

    match state
        .app
        .send_ephemeral_post(&ephemeral.user_id, post)
        .await
    {
        Ok(sent) => match created_post(sent) {
            Ok(response) => response,
            Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
            Err(PrepareError::Unreproducible(why)) => {
                tracing::Span::current().record("forwarded", true);
                tracing::debug!(reason = why, "handing the ephemeral post to Go");
                proxy::forward_to_go(State(state), Request::from_parts(parts, bytes.into())).await
            }
        },
        Err(PrepareError::App(err)) => ApiError::from(err).into_response(),
        Err(PrepareError::Unreproducible(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the ephemeral post to Go");
            proxy::forward_to_go(State(state), Request::from_parts(parts, bytes.into())).await
        }
    }
}

/// Port of `createPostChecks` (api4/post.go:74), whose Go comment asks that any change here be
/// mirrored into `scheduledPostChecks`.
///
/// Six checks, and the second is the one `scheduledPostChecks` does **not** have: a post carrying
/// file ids additionally needs `upload_file` on the channel.
/// Go's `len(post.FileIds) > 0` (api4/post.go:86) — whether a create needs `upload_file`.
///
/// A named function rather than an inline condition because of the middle case. Three inputs,
/// two answers: **no** `file_ids` key and an **empty** list both skip the permission check, and
/// only a non-empty list requires it. `is_some_and(|ids| !ids.is_empty())` collapses the first
/// two, and inverting the emptiness test is invisible from outside the process — separating the
/// arms needs a caller holding `create_post` but **not** `upload_file`, and no stock role on an
/// unlicensed stack grants one without the other (`channel_user` has both). A mutation that
/// inverted it survived a route-level batch for exactly that reason; the unit tests below are
/// what kill it now.
fn post_carries_file_ids(post: &Post) -> bool {
    post.file_ids.as_deref().is_some_and(|ids| !ids.is_empty())
}

async fn create_post_checks(
    where_: &'static str,
    state: &AppState,
    session: &AuthenticatedSession,
    post: &Post,
) -> Result<(), PrepareError> {
    user_create_post_permission_check(state, session, &post.channel_id).await?;

    if post_carries_file_ids(post) {
        let (granted, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &post.channel_id,
                &PERMISSION_UPLOAD_FILE,
            )
            .await;
        if !granted {
            return Err(PrepareError::App(make_permission_error(
                &session.0,
                &[&PERMISSION_UPLOAD_FILE],
            )));
        }
    }

    post_hardened_mode_check(state, session, post.get_props())?;
    post_priority_check(where_, state, session, post).await?;
    post_card_type_check(where_, state, &post.post_type)?;
    state
        .app
        .post_burn_on_read_check(where_, &post.user_id, &post.channel_id, &post.post_type)
        .await
}

/// Port of `postPriorityCheck` (app/post_permission_utils.go:14).
///
/// # `priority == nil` is the whole gate, and everything after it is a refusal
///
/// A post with no `metadata.priority` returns before the user is even fetched. With one, Go reads
/// the user, the two settings and the licence, and every arm ends in an error — there is no
/// "priority allowed" success path that does anything, which is why the checks below are all
/// error branches.
///
/// # The licence arms are 501, not 403
///
/// `requested_ack` and `persistent_notifications` both need at least a Professional licence and
/// answer `license_error.feature_unavailable` at **501** when they do not have one. That is
/// before the `IsPersistentNotificationsEnabled` 403 and before the urgent-priority 400, so an
/// unlicensed server never reaches either.
async fn post_priority_check(
    where_: &'static str,
    state: &AppState,
    session: &AuthenticatedSession,
    post: &Post,
) -> Result<(), PrepareError> {
    let Some(priority) = post.get_priority() else {
        return Ok(());
    };

    // `a.GetUser(userId)`, whose AppError is returned verbatim.
    let user = state.app.get_user(&session.0.user_id).await?;

    let forbidden = || {
        PrepareError::App(AppError::boxed(
            where_,
            "api.post.post_priority.priority_post_not_allowed_for_user.request_error",
            None,
            format!("userId={}", user.id),
            403,
        ))
    };

    if !state.app.config().post_priority {
        return Err(forbidden());
    }

    if !post.root_id.is_empty() {
        return Err(PrepareError::App(AppError::boxed(
            where_,
            "api.post.post_priority.priority_post_only_allowed_for_root_post.request_error",
            None,
            String::new(),
            400,
        )));
    }

    let licensed = state.app.license_state().await? == LicenseState::Licensed;

    if priority.requested_ack == Some(true) {
        if licensed {
            // `MinimumProfessionalLicense` compares the licence's SKU, which lives in the signed
            // body this server never parses.
            return Err(PrepareError::Unreproducible(
                "the acknowledgement gate compares the licence SKU",
            ));
        }
        return Err(license_feature_unavailable(where_));
    }

    if priority.persistent_notifications == Some(true) {
        if licensed {
            return Err(PrepareError::Unreproducible(
                "the persistent-notification gate compares the licence SKU",
            ));
        }
        return Err(license_feature_unavailable(where_));
    }

    Ok(())
}

/// `model.NewAppError("", "license_error.feature_unavailable", nil, "feature is not available for
/// the current license", http.StatusNotImplemented)`.
fn license_feature_unavailable(where_: &'static str) -> PrepareError {
    PrepareError::App(AppError::boxed(
        where_,
        "license_error.feature_unavailable",
        None,
        "feature is not available for the current license",
        501,
    ))
}

/// Port of `PostCardTypeCheckWithApp` (app/post_permission_utils.go:118).
///
/// One branch, and both halves have to hold: the type is `card` **and** `FeatureFlags`
/// `IntegratedBoards` is off. The flag is not in the configuration document either server
/// persists, so a `card` post is forwarded rather than refused — which is the same decision
/// `updatePost`, `patchPost` and `deletePost` already make for the type.
fn post_card_type_check(
    _where: &'static str,
    _state: &AppState,
    post_type: &str,
) -> Result<(), PrepareError> {
    if post_type == POST_TYPE_CARD {
        return Err(PrepareError::Unreproducible(
            "a card post's type check turns on FeatureFlags.IntegratedBoards",
        ));
    }
    Ok(())
}

/// `c.SetInvalidParam(name)` — `api.context.invalid_body_param.app_error` at 400, which is a
/// **different id** from `SetInvalidURLParam`'s.
fn invalid_param_error(name: &'static str) -> Box<AppError> {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert("Name".to_owned(), serde_json::json!(name));
    AppError::boxed(
        "Api4.createPost",
        "api.context.invalid_body_param.app_error",
        Some(params),
        String::new(),
        400,
    )
}

/// `r.URL.Query().Get(key)` — the first value when the key repeats, percent-decoded.
fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    form_urlencoded::parse(query?.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

/// `w.WriteHeader(http.StatusCreated)` then `rp.EncodeJSON(w)` — the same body as
/// [`encoded_post`] with a **201**.
fn created_post(mut post: Post) -> Result<Response, PrepareError> {
    let mut body = Vec::new();
    if let Err(err) = post.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise Post");
        return Err(PrepareError::App(AppError::boxed(
            "createPost",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }
    Ok((
        StatusCode::CREATED,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A router built against a pool that is never connected and a Go upstream that refuses
    /// every connection.
    ///
    /// Neither is a limitation: the requests below carry **no `Authorization` header**, and
    /// `AuthenticatedSession` rejects a tokenless request before it reaches `get_session`, so no
    /// query is ever issued. The refused upstream is the point of the control — it is what makes
    /// "this route falls through to the proxy" a *visible* 502 rather than an indistinguishable
    /// success.
    async fn router_on_a_dead_stack() -> axum::Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool never connects");
        let app = mm_app::App::new(mm_store::SqlStore::from_pool(pool));
        crate::router(AppState::new(app, "http://127.0.0.1:1".to_owned()))
    }

    /// Send one request through the router in this process and return its status.
    async fn status_of(method: &str, path: &str) -> u16 {
        let router = router_on_a_dead_stack().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        let addr = listener.local_addr().expect("the bound address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("a client");
        let status = client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("a method"),
                format!("http://{addr}{path}"),
            )
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("the in-process router answers")
            .status()
            .as_u16();

        server.abort();
        status
    }

    /// A 26-character id, so `RequirePostId` passes and the request reaches the session check.
    const AN_ID: &str = "mmrscreateposts00000000000";

    /// Every `/api/v4/posts…` pair this server answered before `POST /posts` and
    /// `POST /posts/ephemeral` were registered, re-asked through the router built in this
    /// process.
    ///
    /// # The hazard this measures
    ///
    /// axum prefers a **static** segment over `{param}` and does not fall back across method
    /// routers. `/api/v4/posts/ephemeral` is a literal sibling of `/api/v4/posts/{post_id}`, so
    /// registering it with `POST` alone would silently take the `GET`, `PUT` and `DELETE` on that
    /// exact path away from `get_post`, `update_post` and `delete_post` — each of which answers
    /// **400** for the nine-character segment. That is the shape that took the served
    /// `GET /groups/names` out of service; [`crate::invalid_post_id_param`] is the fix and this
    /// is the proof.
    ///
    /// # Reading the expectations
    ///
    /// - **401** — the route is registered and reached `AuthenticatedSession`, which refused a
    ///   request with no token. That is "this server answers it".
    /// - **400** — the route is registered and answered before the session check, which is what
    ///   `RequirePostId` does for a segment that is not 26 characters.
    /// - **502** — nothing matched, so `Router::fallback` tried to proxy to a Go server that is
    ///   not there. That is "this server forwards it", and
    ///   [`a_forwarded_route_is_a_502_here_which_is_what_makes_the_list_above_mean_something`]
    ///   is why a row of 401s is not vacuous.
    #[tokio::test]
    async fn registering_the_two_create_routes_un_serves_nothing() {
        for (method, path, expected) in [
            // The three on `{post_id}` that the `ephemeral` literal could have stolen.
            ("GET", format!("/api/v4/posts/{AN_ID}"), 401),
            ("PUT", format!("/api/v4/posts/{AN_ID}"), 401),
            ("DELETE", format!("/api/v4/posts/{AN_ID}"), 401),
            // The same three at the literal path. 400, not 401 and not 502: `ephemeral` is nine
            // characters, so `RequirePostId` refuses it exactly as `{post_id}` did.
            ("GET", "/api/v4/posts/ephemeral".to_owned(), 400),
            ("PUT", "/api/v4/posts/ephemeral".to_owned(), 400),
            ("DELETE", "/api/v4/posts/ephemeral".to_owned(), 400),
            // The two newly registered POSTs.
            ("POST", "/api/v4/posts".to_owned(), 401),
            ("POST", "/api/v4/posts/ephemeral".to_owned(), 401),
            // The literal that was already here, and the deeper paths.
            ("POST", "/api/v4/posts/ids".to_owned(), 401),
            ("POST", "/api/v4/posts/ids/reactions".to_owned(), 401),
            ("PUT", format!("/api/v4/posts/{AN_ID}/patch"), 401),
            ("GET", format!("/api/v4/posts/{AN_ID}/thread"), 401),
            ("GET", format!("/api/v4/posts/{AN_ID}/reactions"), 401),
            ("GET", format!("/api/v4/posts/{AN_ID}/edit_history"), 401),
            ("POST", format!("/api/v4/posts/{AN_ID}/pin"), 401),
            ("POST", format!("/api/v4/posts/{AN_ID}/unpin"), 401),
            ("GET", format!("/api/v4/posts/{AN_ID}/files/info"), 401),
        ] {
            assert_eq!(
                status_of(method, &path).await,
                expected,
                "{method} {path} stopped being answered by this server"
            );
        }
    }

    /// The control for the test above: a `/posts/…` path this server has never served answers
    /// **502**, because nothing matched and the fallback tried the dead upstream.
    ///
    /// Without this, a bug that un-registered every route at once would leave the list above
    /// full of 401s from some other cause and pass. Checked by hand the other way round too —
    /// adding `POST /api/v4/posts/{post_id}/move` to the list above with an expectation of 401
    /// fails with `502`, which is the vacuity check the list needs.
    #[tokio::test]
    async fn a_forwarded_route_is_a_502_here_which_is_what_makes_the_list_above_mean_something() {
        for (method, path) in [
            ("POST", format!("/api/v4/posts/{AN_ID}/move")),
            ("POST", format!("/api/v4/posts/{AN_ID}/restore/{AN_ID}")),
            ("POST", "/api/v4/posts/rewrite".to_owned()),
            ("POST", "/api/v4/posts/search".to_owned()),
        ] {
            assert_eq!(
                status_of(method, &path).await,
                502,
                "{method} {path} is answered here, so the list above proves less than it claims"
            );
        }
    }

    /// The three inputs of `post_carries_file_ids`, and the middle one is the point.
    ///
    /// An absent `file_ids` and a present-but-empty one are the **same** answer; only a non-empty
    /// list requires `upload_file`. Route-level parity cannot see a swap here, so these are the
    /// tests that pin it — see the function's own doc comment for why.
    #[test]
    fn only_a_non_empty_file_id_list_requires_upload_file() {
        let absent = Post::default();
        assert!(
            absent.file_ids.is_none(),
            "the default post carries no file_ids, which is the case being tested"
        );
        assert!(
            !post_carries_file_ids(&absent),
            "no file_ids key: the upload_file check is skipped"
        );

        let empty = Post {
            file_ids: Some(Vec::new()),
            ..Post::default()
        };
        assert!(
            !post_carries_file_ids(&empty),
            "an empty list is len 0 in Go too: the check is skipped"
        );

        let one = Post {
            file_ids: Some(vec!["kbcdefghijklmnopqrstuvwxyz".to_owned()]),
            ..Post::default()
        };
        assert!(post_carries_file_ids(&one), "one id: the check fires");

        let several = Post {
            file_ids: Some(vec![
                "kbcdefghijklmnopqrstuvwxyz".to_owned(),
                "lbcdefghijklmnopqrstuvwxyz".to_owned(),
            ]),
            ..Post::default()
        };
        assert!(post_carries_file_ids(&several), "two ids: the check fires");
    }

    #[test]
    fn set_online_swallows_its_parse_error_and_silent_does_not() {
        // Not the handler — the two `strconv.ParseBool` calls it makes, whose *error handling* is
        // opposite. `parse_go_bool` returning `None` is the branch both of them take.
        assert_eq!(parse_go_bool("bogus"), None);
        assert_eq!(parse_go_bool("1"), Some(true));
        assert_eq!(parse_go_bool("True"), Some(true));
        // Go's list is case-sensitive apart from the six spellings, so `yes` and `TrUe` are not
        // booleans — `set_online=yes` stays true and `silent=yes` is a 400.
        assert_eq!(parse_go_bool("yes"), None);
        assert_eq!(parse_go_bool("TrUe"), None);
    }

    #[test]
    fn the_query_reader_takes_the_first_value_when_a_key_repeats() {
        // `url.Values.Get` returns `v[0]`.
        assert_eq!(
            query_value(Some("silent=true&silent=false"), "silent").as_deref(),
            Some("true")
        );
        // A bare key is the empty string, which both call sites treat as "absent".
        assert_eq!(query_value(Some("silent"), "silent").as_deref(), Some(""));
        assert_eq!(query_value(Some("other=1"), "silent"), None);
        assert_eq!(query_value(None, "silent"), None);
        // Percent-decoding happens before ParseBool sees the value.
        assert_eq!(
            query_value(Some("set_online=%74rue"), "set_online").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn the_two_invalid_param_ids_are_not_the_same_error() {
        // `SetInvalidParam` is `api.context.invalid_body_param.app_error`; `SetInvalidURLParam` is
        // `api.context.invalid_url_param.app_error`. `?silent=bogus` raises the first, and a
        // nine-character post id raises the second — clients branch on the id.
        let silent = invalid_param_error("silent");
        assert_eq!(silent.id, "api.context.invalid_body_param.app_error");
        assert_eq!(silent.status_code, 400);
        assert_eq!(
            silent.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::json!("silent"))
        );
    }

    #[tokio::test]
    async fn a_card_post_is_forwarded_rather_than_refused() {
        // `PostCardTypeCheckWithApp` answers 400 only when `FeatureFlags.IntegratedBoards` is
        // *off*, and the flag is in neither configuration document. Answering the 400 would be a
        // guess about a flag; forwarding is the same decision `updatePost` and `deletePost` make.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool never connects");
        let state = AppState::new(
            mm_app::App::new(mm_store::SqlStore::from_pool(pool)),
            "http://127.0.0.1:1".to_owned(),
        );
        assert!(matches!(
            post_card_type_check("Api4.createPost", &state, POST_TYPE_CARD),
            Err(PrepareError::Unreproducible(_))
        ));
        assert!(post_card_type_check("Api4.createPost", &state, "").is_ok());
        // Only the exact type. `card_something` is a different post type and is not this branch.
        assert!(post_card_type_check("Api4.createPost", &state, "cardigan").is_ok());
    }
}
