//! Port of `api4/post.go`'s `getPost` — `GET /api/v4/posts/{post_id}`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::response::{IntoResponse, Response};
use mm_app::post::{PrepareError, PreparePostForClientOpts};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_READ_DELETED_POSTS,
    make_permission_error,
};
use mm_model::utils::is_valid_id;
use mm_store::post_store::GetPostsOptions;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_first, query_flag_is_true};
use crate::error::ApiError;
use crate::proxy;

/// `model.HeaderEtagServer`. Go's constant is the literal `"ETag"`.
const HEADER_ETAG_SERVER: &str = "ETag";

/// `getPost`'s only query parameter (api4/post.go:581).
const INCLUDE_DELETED_PARAM: &str = "include_deleted";

/// What the handler decided to do, before any of it is written.
enum Outcome {
    Served(Response),
    Failed(ApiError),
    /// The Go server has to answer this one — see [`mm_app::post`] for the shapes and why.
    Forward,
}

/// Port of `getPost` (api4/post.go:575).
///
/// # Order is wire format
///
/// `RequirePostId` → the `include_deleted` gate → `GetPostIfAuthorized` → prepare → sanitize →
/// etag. Moving the `include_deleted` gate after the fetch would turn a non-admin's request for
/// a **missing** post from a 403 into a 404, which is exactly the kind of information leak the
/// ordering exists to prevent.
///
/// # Two headers, and only one of them is reachable
///
/// `ETag` is set on the 200 **and** on the 304 (`HandleEtag`, web/context.go:236), from
/// `post.Etag()` = `<CurrentVersion>.<id>.<update_at>` — a raw string, not a quoted or weak
/// entity tag, compared against `If-None-Match` byte for byte.
///
/// `First-Inaccessible-Post-Time: 1` is set only when `GetPostIfAuthorized` fails with
/// `app.post.cloud.get.app_error`, and that error needs a licence carrying a `PostHistory`
/// limit (app/post.go:2166). **This deployment cannot produce it**, so the branch is not
/// reproduced here rather than being written blind against an oracle that cannot be run. If a
/// Cloud licence ever lands, the header goes back beside that error id.
///
/// # The audit record is not ported
///
/// Go builds one at the end, tagging `non_channel_member_access` when either `isMember` or
/// `previewIsMember` is false. Both booleans are computed and returned by the app layer — and
/// then discarded here, because there is no audit layer to hand them to ([D-028]). They are
/// bound to `_`-prefixed names rather than dropped from the signatures so that the audit record,
/// when it lands, has its two inputs already in the right place.
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // Copied out before the request is consumed, so the forward path can still hand Go the
    // untouched original.
    let query = request.uri().query().map(str::to_owned);
    let if_none_match = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match serve(&state, &post_id, &session, query.as_deref(), if_none_match).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

async fn serve(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    if_none_match: Option<String>,
) -> Outcome {
    // `c.RequirePostId()` (web/context.go:411). The router's `[A-Za-z0-9]+` charset has already
    // rejected the shapes gorilla would 404, so what is left for this to catch is a segment of
    // the wrong *length*.
    if !is_valid_id(post_id) {
        return Outcome::Failed(ApiError::invalid_url_param("post_id"));
    }

    // `strconv.ParseBool` with the error **discarded** — `?include_deleted=yes` is `false`, not
    // a 400.
    let include_deleted = query_flag_is_true(query, INCLUDE_DELETED_PARAM);

    // The gate is `manage_system` on the *session's system roles*, not on the channel.
    if include_deleted
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Outcome::Failed(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let (post, _is_member) = match state
        .app
        .get_post_if_authorized(post_id, &session.0, include_deleted)
        .await
    {
        Ok(found) => found,
        Err(err) => return Outcome::Failed(ApiError(*err)),
    };

    // `&model.PreparePostForClientOpts{IncludePriority: true}` — every other field is false, and
    // `IncludeDeleted` staying false while the *query parameter* is true is Go's, not an
    // oversight to tidy: a deleted post's file infos are still filtered to `DeleteAt = 0`.
    let opts = PreparePostForClientOpts {
        include_priority: true,
        ..PreparePostForClientOpts::default()
    };

    let prepared = match state
        .app
        .prepare_post_for_client_with_embeds_and_images(&post, opts)
        .await
    {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, post_id = %post_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError(*err)),
    };

    let (mut post, _preview_is_member) = match state
        .app
        .sanitize_post_metadata_for_user(prepared, &session.0.user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, post_id = %post_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError(*err)),
    };

    // `c.HandleEtag(post.Etag(), ...)`: an exact string compare, no weak comparison and no
    // candidate list, and the 304 carries the etag back.
    let etag = post.etag();
    if if_none_match.as_deref() == Some(etag.as_str()) {
        return Outcome::Served(
            (
                StatusCode::NOT_MODIFIED,
                [(ETAG.as_str(), etag.as_str()), ("x-mmrs-served-by", "rust")],
            )
                .into_response(),
        );
    }

    // `post.EncodeJSON(w)` — strips the private action integrations **in place** and appends the
    // newline `json.Encoder` writes and `json.Marshal` does not. Both live in the model.
    let mut body = Vec::new();
    if let Err(err) = post.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise Post");
        return Outcome::Failed(ApiError(mm_model::utils::AppError::new(
            "getPost",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }

    Outcome::Served(
        (
            StatusCode::OK,
            [
                (HEADER_ETAG_SERVER, etag.as_str()),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

/// The three query parameters that choose a branch of `getPostsForChannel`, and one that
/// changes what a chosen branch returns.
const AFTER_PARAM: &str = "after";
const BEFORE_PARAM: &str = "before";
const SINCE_PARAM: &str = "since";
const SKIP_FETCH_THREADS_PARAM: &str = "skipFetchThreads";
const COLLAPSED_THREADS_PARAM: &str = "collapsedThreads";
const COLLAPSED_THREADS_EXTENDED_PARAM: &str = "collapsedThreadsExtended";

/// Port of `getPostsForChannel` (api4/post.go:271) — `GET /api/v4/channels/{channel_id}/posts`.
///
/// # One handler, four store branches; this serves one of them
///
/// `since`, `after` and `before` each select a different query, and the fourth — the plain page
/// — is what a client asks for when it opens a channel. Only the page branch is served here.
/// The other three are **forwarded before anything else happens**, including before their own
/// validation, so Go answers their 400s as well as their bodies. `since=0` is not one of them:
/// Go's test is `since > 0`, so an explicit zero falls through to the page branch, etag and all.
///
/// `collapsedThreadsExtended=true` is forwarded for a different reason: it replaces each stub
/// thread participant with a profile run through `SanitizeProfile`, whose output depends on
/// config this server does not read yet.
///
/// # `collapsedThreads` changes the answer in three ways, and one of them is not a filter
///
/// With it on, the window is roots only, the reply count and participants come from `Threads`
/// rather than a subquery, and — the one that looks like a bug in a diff — `MakeNonNil` is *not*
/// applied, so a post with no props serialises `"props":null` here and `"props":{}` on a plain
/// page. See [`mm_store::PostStore::get_posts`].
///
/// # The etag is not what the branch it guards returns
///
/// It is `CurrentVersion.<newest UpdateAt in the channel>` regardless of `collapsedThreads`
/// (Go drops that filter — see [`mm_app::App::get_posts_etag`]) and regardless of `page`. So
/// every page of a channel shares one etag, and a client that pages backwards with
/// `If-None-Match` set from page 0 gets a 304 for page 1. That is Go's behaviour, reproduced.
///
/// # The audit record is not ported
///
/// As in [`get_post`]: `isMember` and `isMemberForAllPreviews` are computed and dropped ([D-028]).
#[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
pub async fn get_posts_for_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    let if_none_match = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match serve_channel_posts(
        &state,
        &channel_id,
        &session,
        query.as_deref(),
        if_none_match,
    )
    .await
    {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

async fn serve_channel_posts(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    if_none_match: Option<String>,
) -> Outcome {
    // `c.RequireChannelId()` (web/context.go:377).
    if !is_valid_id(channel_id) {
        return Outcome::Failed(ApiError::invalid_url_param("channel_id"));
    }

    // The three cursor branches, forwarded whole. Note the difference between "absent" and
    // "empty": `?after=` is `""` in Go too and takes the page branch, so the test is on the
    // value rather than on the key being present.
    if query_first(query, AFTER_PARAM).is_some_and(|value| !value.is_empty())
        || query_first(query, BEFORE_PARAM).is_some_and(|value| !value.is_empty())
    {
        return Outcome::Forward;
    }
    // `since` picks its branch only when **positive** — Go's test is `since > 0` — so `?since=0`
    // falls through to the page branch, etag and all. A value that does not parse is Go's 400,
    // whose detail string wraps strconv's own error text; forwarding is how a client gets it.
    if let Some(since) = query_first(query, SINCE_PARAM).filter(|value| !value.is_empty()) {
        match since.parse::<i64>() {
            Ok(since) if since > 0 => return Outcome::Forward,
            Err(_) => return Outcome::Forward,
            Ok(_) => {}
        }
    }

    let collapsed_threads = query_flag_is_true(query, COLLAPSED_THREADS_PARAM);
    if query_flag_is_true(query, COLLAPSED_THREADS_EXTENDED_PARAM) {
        return Outcome::Forward;
    }
    let skip_fetch_threads = query_flag_is_true(query, SKIP_FETCH_THREADS_PARAM);
    let include_deleted = query_flag_is_true(query, INCLUDE_DELETED_PARAM);

    // `!c.IsSystemAdmin() && includeDeleted` — the *check* is `manage_system`, the permission the
    // 403 **names** is `read_deleted_posts`, and the two are not the same string. A client
    // branching on `id` sees the latter.
    if include_deleted
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Outcome::Failed(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_DELETED_POSTS],
        )));
    }

    let channel = match state.app.get_channel(channel_id).await {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(ApiError(err)),
    };

    // Unlike `GetPostIfAuthorized`, there is **no second `read_public_channel` fallback** here,
    // so an open channel the caller is not a member of refuses with `read_channel_content`.
    let (has_permission, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;
    if !has_permission {
        return Outcome::Failed(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    let etag = state.app.get_posts_etag(channel_id).await;
    if if_none_match.as_deref() == Some(etag.as_str()) {
        return Outcome::Served(
            (
                StatusCode::NOT_MODIFIED,
                [(ETAG.as_str(), etag.as_str()), ("x-mmrs-served-by", "rust")],
            )
                .into_response(),
        );
    }

    let opts = GetPostsOptions {
        channel_id,
        user_id: &session.0.user_id,
        page: parse_page(query),
        per_page: parse_per_page(query),
        skip_fetch_threads,
        collapsed_threads,
        include_deleted,
    };
    let list = match state.app.get_posts_page(opts).await {
        Ok(list) => list,
        Err(err) => return Outcome::Failed(ApiError(err)),
    };

    let mut prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError(*err)),
    };

    // `AddCursorIdsForPostList(list, userID, afterPost, beforePost, since, page, perPage, ...)`
    // with `afterPost == ""`, `beforePost == ""` and `since == 0` — every conditional in it is
    // false, so both cursors come from the list itself. The branches that read `page` and
    // `perPage` belong to the forwarded requests and are deliberately not ported.
    prepared.next_post_id = state
        .app
        .get_next_post_id_from_post_list(&prepared, &session.0.user_id, collapsed_threads)
        .await;
    prepared.prev_post_id = state
        .app
        .get_prev_post_id_from_post_list(&prepared, &session.0.user_id, collapsed_threads)
        .await;

    let (mut sanitized, _all_previews_have_membership) = match state
        .app
        .sanitize_post_list_metadata_for_user(prepared, &session.0.user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError(*err)),
    };

    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise PostList");
        return Outcome::Failed(ApiError(mm_model::utils::AppError::new(
            "getPostsForChannel",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }

    Outcome::Served(
        (
            StatusCode::OK,
            [
                (HEADER_ETAG_SERVER, etag.as_str()),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}
