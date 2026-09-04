//! Port of `api4/post.go`'s post reads: `getPost` (`GET /api/v4/posts/{post_id}`),
//! `getPostsForChannel` (`GET /api/v4/channels/{channel_id}/posts`), `getPostThread`
//! (`GET /api/v4/posts/{post_id}/thread`), `getFileInfosForPost`
//! (`GET /api/v4/posts/{post_id}/files/info`), `getEditHistoryForPost`
//! (`GET /api/v4/posts/{post_id}/edit_history`), `getPostsForChannelAroundLastUnread`
//! (`GET /api/v4/users/{user_id}/channels/{channel_id}/posts/unread`), `getPostsByIds`
//! (`POST /api/v4/posts/ids`) and `getFlaggedPostsForUser`
//! (`GET /api/v4/users/{user_id}/posts/flagged`).

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::response::{IntoResponse, Response};
use mm_app::post::{PrepareError, PreparePostForClientOpts};
use mm_model::file_info::get_etag_for_file_infos;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_EDIT_POST, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_READ_CHANNEL_CONTENT, PERMISSION_READ_DELETED_POSTS, make_permission_error,
};
use mm_model::utils::{PAYLOAD_PARSE_ERROR, go_json_marshal, is_valid_id, sorted_array_from_json};
use mm_store::post_store::{GetPostThreadOptions, GetPostsOptions, ThreadDirection};

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
        return Outcome::Failed(ApiError::from(make_permission_error(
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
        Err(err) => return Outcome::Failed(ApiError::from(err)),
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
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
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
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
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
        return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
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
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_DELETED_POSTS],
        )));
    }

    let channel = match state.app.get_channel(channel_id).await {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    // Unlike `GetPostIfAuthorized`, there is **no second `read_public_channel` fallback** here,
    // so an open channel the caller is not a member of refuses with `read_channel_content`.
    let (has_permission, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;
    if !has_permission {
        return Outcome::Failed(ApiError::from(make_permission_error(
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
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    let mut prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
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
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise PostList");
        return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
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

/// `getPostThread`'s six query parameters (api4/post.go:812).
const PER_PAGE_PARAM: &str = "perPage";
const FROM_CREATE_AT_PARAM: &str = "fromCreateAt";
const FROM_POST_PARAM: &str = "fromPost";
const FROM_UPDATE_AT_PARAM: &str = "fromUpdateAt";
const UPDATES_ONLY_PARAM: &str = "updatesOnly";
const DIRECTION_PARAM: &str = "direction";

/// `web.PerPageMaximum` (channels/web/params.go:20).
///
/// This route **rejects** a larger value with a 400 where every paginated route ported so far
/// clamps it silently. `parse_per_page` is therefore the wrong helper here, and reusing it
/// would turn Go's 400 into a 200.
const PER_PAGE_MAXIMUM: i64 = 200;

/// `r.URL.Query().Get(flag) == "true"` — and that is **not** [`query_flag_is_true`].
///
/// `getPostThread` compares the raw string; its neighbour `getPostsForChannel` calls
/// `strconv.ParseBool` on the same four parameter names, forty lines earlier in the same file.
/// So `?skipFetchThreads=1` is **true** for the channel page and **false** for the thread, and
/// the same goes for `t`, `TRUE` and `T`. Measured against the running 11.11.0 server on all
/// four spellings, because a shared helper here is exactly the shortcut that would look right.
fn query_flag_is_literally_true(query: Option<&str>, flag: &str) -> bool {
    query_first(query, flag).is_some_and(|value| value == "true")
}

/// Go's `SetInvalidParam` argument at api4/post.go:843, where the whole sentence is passed as
/// the *parameter name*.
///
/// It reaches `AppError.params["Name"]`, which is unexported in Go and `#[serde(skip)]` here —
/// and then reaches the client anyway, interpolated into the translated `message`. Go really does
/// answer `Invalid or missing if fromPost is set, then fromCreateAt must also be set in request
/// body.`; the sentence is not internal. See [`get_post_thread`].
const FROM_POST_NEEDS_FROM_CREATE_AT: &str =
    "if fromPost is set, then fromCreateAt must also be set";

/// Port of `getPostThread` (api4/post.go:812) — `GET /api/v4/posts/{post_id}/thread`.
///
/// # Nine validation branches, one error id — and the parameter name is on the wire
///
/// Go spells its 400s three ways — `SetInvalidParam`, `SetInvalidParamWithErr` and
/// `SetInvalidParamWithDetails` — and all three build `api.context.invalid_body_param.app_error`,
/// differing only in `DetailedError` and in the unexported params map. `handleContextError` wipes
/// `DetailedError` unless `EnableDeveloper` is set, and the map is `json:"-"`, so it is tempting
/// to conclude the nine failures are indistinguishable. **They are not.** `Translate` interpolates
/// `params["Name"]` into `message`, so Go answers `Invalid or missing perPage in request body.`
/// and, for the branch below whose "parameter" is a whole sentence, `Invalid or missing if
/// fromPost is set, then fromCreateAt must also be set in request body.` — measured on all nine.
///
/// Our `message` is the untranslated id for every one of them, which is [D-092] and not specific
/// to this route. That makes the name each branch passes a **latent wire value**: it is wrong
/// today only in the way every id on this server is wrong, and it becomes right the day i18n
/// lands. `parity/post_thread.rs` asserts Go's nine messages so a branch wired to the wrong name
/// is caught now rather than then.
///
/// # The permission check runs after the query
///
/// `GetPostThread` is called first and `GetPostIfAuthorized` second (api4/post.go:893 against
/// :918), so a caller with no access to the channel still causes the thread to be read, and a
/// **missing** post is a 404 for everyone rather than a 403 — the reverse of the ordering
/// [`get_post`] documents. That is Go's, not an oversight to tidy: swapping them would change
/// the status a client sees.
///
/// # Three forwards
///
/// - `collapsedThreadsExtended=true`, for the reason it is forwarded from
///   [`get_posts_for_channel`]: it replaces each stub participant with a `SanitizeProfile`d
///   user, whose output depends on config this server does not read.
/// - A **negative** `perPage`. `Limit(uint64(perPage + 1))` makes `-1` a `LIMIT 0`, whereupon
///   Go matches zero rows against `perPage+1 == 0`, sets `has_next` and panics on
///   `posts[:len(posts)-1]` — measured: the connection is closed with no response, and the
///   server logs `slice bounds out of range [:-1]` at post_store.go:895. `-2` and below render
///   a `uint64` too large for a Postgres `LIMIT` and come back as a 500 (`pq: bigint out of
///   range`). Forwarding is how a client keeps getting Go's answer, panic included; a port that
///   "fixed" either one would diverge.
/// - Any thread carrying a burn-on-read post, refused by the metadata pipeline as in
///   [`get_posts_for_channel`]. Unlike the channel page, `SqlPostStore.Get` really does populate
///   `BurnOnReadPosts` — see [`mm_app::App::get_post_thread`].
///
/// # Two etags, computed from two different lists
///
/// `HandleEtag` compares `If-None-Match` against the **raw** list's etag and the 200's header
/// carries the **sanitized** list's. Nothing in `PreparePostListForClient` or
/// `SanitizePostListMetadataForUser` touches `order`, an id or an `update_at`, so the two agree
/// — but they are two calls in Go and they are two calls here, because the day one of those
/// stages drops a post the header has to move with it.
///
/// # `FirstInaccessiblePostTime` and the missing-post 400 are both dead here
///
/// Go answers a truncated list with a bare `{"order":[],...}` body and a 200. The field is only
/// ever set by `filterInaccessiblePosts`, which returns immediately without a licence carrying a
/// `PostHistory` limit, so it is always `0` on this deployment and the branch cannot be reached
/// — not reproduced, for the reason [`get_post`] gives about its `First-Inaccessible-Post-Time`
/// header. The `list.Posts[postId]` miss below it is likewise unreachable: both store branches
/// add the requested post before anything else can filter it. It is ported because it is
/// cheap and because it is the handler's only `SetInvalidURLParam` after the router has run.
///
/// # The audit record is not ported
///
/// As in [`get_post`]: `isMember` and `isMemberForAllPreviews` are computed and dropped
/// ([D-028]).
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_post_thread(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    let if_none_match = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match serve_post_thread(&state, &post_id, &session, query.as_deref(), if_none_match).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

async fn serve_post_thread(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    if_none_match: Option<String>,
) -> Outcome {
    // `c.RequirePostId()` (web/context.go:411).
    if !is_valid_id(post_id) {
        return Outcome::Failed(ApiError::invalid_url_param("post_id"));
    }

    // `perPage := 0` and the comment above it: the default is **all items**, kept for mobile.
    // Note the guard is `err != nil || perPage > PerPageMaximum` — a *negative* value passes
    // validation here and is dealt with below.
    let mut per_page = 0_i64;
    if let Some(raw) = query_first(query, PER_PAGE_PARAM).filter(|v| !v.is_empty()) {
        match raw.parse::<i64>() {
            Ok(value) if value <= PER_PAGE_MAXIMUM => per_page = value,
            _ => return Outcome::Failed(ApiError::invalid_param(PER_PAGE_PARAM)),
        }
    }

    let mut from_create_at = 0_i64;
    if let Some(raw) = query_first(query, FROM_CREATE_AT_PARAM).filter(|v| !v.is_empty()) {
        match raw.parse::<i64>() {
            Ok(value) => from_create_at = value,
            Err(_) => return Outcome::Failed(ApiError::invalid_param(FROM_CREATE_AT_PARAM)),
        }
    }

    // `fromPost` has no validation of its own — it is not required to be an id, and an
    // unknown one simply matches nothing in the cursor's tie-break.
    let from_post = query_first(query, FROM_POST_PARAM).unwrap_or_default();
    if !from_post.is_empty() && from_create_at == 0 {
        return Outcome::Failed(ApiError::invalid_param(FROM_POST_NEEDS_FROM_CREATE_AT));
    }

    let mut from_update_at = 0_i64;
    if let Some(raw) = query_first(query, FROM_UPDATE_AT_PARAM).filter(|v| !v.is_empty()) {
        match raw.parse::<i64>() {
            Ok(value) => from_update_at = value,
            Err(_) => return Outcome::Failed(ApiError::invalid_param(FROM_UPDATE_AT_PARAM)),
        }
    }

    // The two cursors are mutually exclusive — the store would apply both predicates.
    if from_update_at != 0 && from_create_at != 0 {
        return Outcome::Failed(ApiError::invalid_param(FROM_UPDATE_AT_PARAM));
    }

    let updates_only = query_flag_is_literally_true(query, UPDATES_ONLY_PARAM);
    if updates_only && from_update_at == 0 {
        return Outcome::Failed(ApiError::invalid_param(FROM_UPDATE_AT_PARAM));
    }

    // An empty `direction` is absent, not invalid — `if dir := q.Get(...); dir != ""`.
    let direction = match query_first(query, DIRECTION_PARAM).filter(|v| !v.is_empty()) {
        None => ThreadDirection::Unset,
        Some(value) if value == "up" => ThreadDirection::Up,
        Some(value) if value == "down" => ThreadDirection::Down,
        Some(_) => return Outcome::Failed(ApiError::invalid_param(DIRECTION_PARAM)),
    };

    // Scrolling up means reading backwards, and "what changed since" only runs forwards.
    if updates_only && direction == ThreadDirection::Up {
        return Outcome::Failed(ApiError::invalid_param(UPDATES_ONLY_PARAM));
    }

    // Both forwards sit **after** validation, which costs nothing and keeps the two servers
    // agreed on a request that is invalid *and* forwarded: Go re-runs every check above before
    // it reaches either of these, so a bad `direction` beside `perPage=-1` is a 400 on both
    // sides rather than a panic on one.
    if query_flag_is_literally_true(query, COLLAPSED_THREADS_EXTENDED_PARAM) {
        return Outcome::Forward;
    }
    if per_page < 0 {
        return Outcome::Forward;
    }

    let collapsed_threads = query_flag_is_literally_true(query, COLLAPSED_THREADS_PARAM);
    let skip_fetch_threads = query_flag_is_literally_true(query, SKIP_FETCH_THREADS_PARAM);

    let opts = GetPostThreadOptions {
        user_id: &session.0.user_id,
        skip_fetch_threads,
        collapsed_threads,
        updates_only,
        per_page,
        direction,
        from_post: &from_post,
        from_create_at,
        from_update_at,
    };

    let list = match state.app.get_post_thread(post_id, opts).await {
        Ok(list) => list,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    // `post, ok := list.Posts[c.Params.PostId]` — see the doc comment on why `ok` cannot be
    // false through this route.
    if !list
        .posts
        .as_ref()
        .is_some_and(|posts| posts.contains_key(post_id))
    {
        return Outcome::Failed(ApiError::invalid_url_param("post_id"));
    }

    // `includeDeleted` is hard-coded `false` here, so this second fetch of the same post cannot
    // see a row the thread query did not.
    let (_post, _is_member) = match state
        .app
        .get_post_if_authorized(post_id, &session.0, false)
        .await
    {
        Ok(found) => found,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    let etag = list.etag();
    if if_none_match.as_deref() == Some(etag.as_str()) {
        return Outcome::Served(
            (
                StatusCode::NOT_MODIFIED,
                [(ETAG.as_str(), etag.as_str()), ("x-mmrs-served-by", "rust")],
            )
                .into_response(),
        );
    }

    let prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, post_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let (mut sanitized, _all_previews_have_membership) = match state
        .app
        .sanitize_post_list_metadata_for_user(prepared, &session.0.user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, post_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // Deliberately the sanitized list's own etag rather than the one compared above.
    let response_etag = sanitized.etag();

    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise PostList");
        return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
            "getPostThread",
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
                (HEADER_ETAG_SERVER, response_etag.as_str()),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

/// `getFileInfosForPost` and `getFileInfo` both set this, and only on the 200.
///
/// A month, marked private so a shared cache cannot hold one user's file metadata for another.
/// `HandleEtag` writes the 304 before either handler reaches its own header block, so a 304 from
/// these routes carries `ETag` and **no** `Cache-Control` — which is why the constant is applied
/// at the two 200 sites rather than folded into a shared response builder.
pub(crate) const FILE_CACHE_CONTROL: &str = "max-age=2592000, private";

/// Port of `getFileInfosForPost` (api4/post.go:1583).
///
/// # Order, and the one gate that is missing from it
///
/// `RequirePostId` → `SessionHasPermissionToReadPost` → the `include_deleted` `manage_system`
/// gate → the file infos → etag. Note what is **not** first: unlike `getPost`, the permission
/// check runs *before* the `include_deleted` gate, so a non-admin asking for deleted files on a
/// post they cannot read is refused with `read_channel_content`, not `manage_system`.
///
/// # Go fetches the post twice and we fetch it once
///
/// `FeatureFlags.PermissionPolicies` defaults to **true** (feature_flags.go:172), so Go's
/// ABAC block runs: it calls `GetSinglePost` and then `HasPermissionToFileAction`. The second
/// of those is unconditionally `true` here — see [`mm_app::file::has_permission_to_file_action`]
/// — and the first raises `app.post.get.app_error`/404 for a missing post, which is *the same
/// error id and status* [`mm_app::App::get_file_infos_for_post_with_migration`] raises from its
/// own `GetSingle` a few lines later. `AppError.Where` is `json:"-"`, so nothing distinguishes
/// them on the wire and the duplicate read is dropped rather than reproduced.
///
/// # The empty answer is `null`, not `[]` — the same trap as `getReactions`
///
/// Follow the nil the whole way down. `SqlFileInfoStore.GetByIds` short-circuits
/// `if len(items) == 0 { return nil, nil }` (file_info_store.go:161), so zero rows is a **nil**
/// slice rather than an empty one. `orderFileInfosByID` returns its argument untouched for fewer
/// than two infos, `removeInaccessibleContentFromFilesSlice` returns early on length zero, and
/// `generateMiniPreviewForInfos` ranges over nothing — none of the three materialises a slice.
/// `json.Marshal` of a nil `[]*model.FileInfo` is `null`.
///
/// So a post with no attachments answers the four bytes `null`, which is the common case for
/// this route as it is for `getReactions`. `serde_json` would render an empty `Vec` as `[]`,
/// which is why the empty case is spelled out below rather than left to the serialiser.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write`, so **no trailing newline** — unlike `getFileInfo`, its sibling
/// one crate over, which encodes and gets one. The 200 carries `ETag` and
/// `Cache-Control: max-age=2592000, private`; the 304 carries `ETag` alone.
///
/// # The etag is the *file infos'*, not the post's — and the empty one is a global constant
///
/// `GetEtagForFileInfos` (model/file_info.go:231) is `<version>.<infos[0].postId>.<max updateAt
/// across the whole list>`; note the two halves can come from different elements. For an **empty**
/// list it is a bare `model.Etag()` with no parts at all — `CurrentVersion` and nothing else — so
/// every post with no attachments, anywhere on the server, shares one etag and a client that
/// cached one empty list gets a 304 for every other. Measured against Go, because the intuitive
/// reading (an empty list must be uncacheable) is exactly backwards.
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_file_infos_for_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    let if_none_match = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match serve_file_infos_for_post(&state, &post_id, &session, query.as_deref(), if_none_match)
        .await
    {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

async fn serve_file_infos_for_post(
    state: &AppState,
    post_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    if_none_match: Option<String>,
) -> Outcome {
    // `c.RequirePostId()` (web/context.go:411).
    if !is_valid_id(post_id) {
        return Outcome::Failed(ApiError::invalid_url_param("post_id"));
    }

    // Before the `include_deleted` gate, which is the reverse of `getPost`.
    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_read_post(&session.0, post_id)
        .await;
    if !allowed {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    let include_deleted = query_flag_is_true(query, INCLUDE_DELETED_PARAM);
    if include_deleted
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    // `HasPermissionToFileAction` — unconditionally true on this deployment; kept at Go's call
    // site so the gate is already in place if an evaluator ever exists.
    if !mm_app::file::has_permission_to_file_action() {
        return Outcome::Failed(ApiError::from(*mm_app::file::abac_denied(
            "getFileInfosForPost",
        )));
    }

    let infos = match state
        .app
        .get_file_infos_for_post_with_migration(post_id, include_deleted)
        .await
    {
        Ok(infos) => infos,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, post_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let etag = get_etag_for_file_infos(&infos);
    if if_none_match.as_deref() == Some(etag.as_str()) {
        return Outcome::Served(
            (
                StatusCode::NOT_MODIFIED,
                [(ETAG.as_str(), etag.as_str()), ("x-mmrs-served-by", "rust")],
            )
                .into_response(),
        );
    }

    // A nil slice, not an empty one — see the doc comment.
    let body = if infos.is_empty() {
        b"null".to_vec()
    } else {
        match serde_json::to_vec(&infos) {
            Ok(body) => body,
            Err(err) => {
                tracing::error!(error = %err, "failed to serialise FileInfos");
                return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
                    "getFileInfosForPost",
                    "api.marshal_error",
                    None,
                    String::new(),
                    500,
                )));
            }
        }
    };

    Outcome::Served(
        (
            StatusCode::OK,
            [
                (HEADER_ETAG_SERVER, etag.as_str()),
                ("Cache-Control", FILE_CACHE_CONTROL),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

/// `getPostsForChannelAroundLastUnread`'s three boolean query parameters (api4/post.go:419).
const SKIP_FETCH_THREADS_UNREAD_PARAM: &str = "skipFetchThreads";
const COLLAPSED_THREADS_UNREAD_PARAM: &str = "collapsedThreads";

/// `web.LimitDefault` (params.go:23).
const LIMIT_DEFAULT: i64 = 60;
/// `web.LimitMaximum` (params.go:24).
const LIMIT_MAXIMUM: i64 = 200;

/// The `limit_after`/`limit_before` half of `web.ParamsFromRequest` (params.go:251).
///
/// Clamped, never refused: garbage and negatives fall to 60, anything over 200 becomes 200. So
/// the **only** value that reaches the handler's own `limit_after == 0` check is an explicit
/// `?limit_after=0`, which is why that 400 exists at all.
fn parse_limit(query: Option<&str>, key: &str) -> i64 {
    match query_first(query, key).and_then(|v| v.parse::<i64>().ok()) {
        Some(val) if val < 0 => LIMIT_DEFAULT,
        Some(val) if val > LIMIT_MAXIMUM => LIMIT_MAXIMUM,
        Some(val) => val,
        None => LIMIT_DEFAULT,
    }
}

/// Port of `getPostsForChannelAroundLastUnread` (api4/post.go:390), reached as
/// `GET /api/v4/users/{user_id}/channels/{channel_id}/posts/unread`.
///
/// # The validation order is the reverse of its sibling's
///
/// `c.RequireUserId().RequireChannelId()` — **user first**, where `getChannelUnread` one route
/// over opens `c.RequireChannelId().RequireUserId()`. Both hang off `BaseRoutes.ChannelForUser`,
/// so the path segments arrive in the same order and only the handlers disagree. With both ids
/// malformed the two routes name different parameters in their 400.
///
/// # Two gates, and the second one takes the channel
///
/// `SessionHasPermissionToUser` first — asking about yourself always passes — and then
/// `SessionHasPermissionToReadChannel`, which needs the `Channel` and so runs *after* the
/// `GetChannel` that can 404. The refusals report `edit_other_users` and `read_channel_content`.
/// Note the second is **not** the `read_channel` `getChannelUnread` reports for the same channel.
///
/// # `limit_after == 0` is the one pagination value that is a 400
///
/// `web.ParamsFromRequest` clamps everything else — negatives and garbage to 60, over-200 to
/// 200 — so the check can only fire for a literal `?limit_after=0`. `limit_before=0` is
/// perfectly legal and asks for no history at all.
///
/// # The empty-list fallback, and the etag that only exists inside it
///
/// When the around-query comes back with an empty `order` — the member has never viewed the
/// channel, or has read everything — Go **discards it and re-fetches a plain first page** of
/// `limitBefore` posts. Only that branch computes an etag, and only that branch sets the `ETag`
/// header, so a client that *does* have unread posts gets no etag at all and can never be
/// answered 304. The etag is `GetPostsEtag`, whose `collapsedThreads` argument Go drops on the
/// floor — see [`mm_app::App::get_posts_etag`].
///
/// The fallback's `UserId` is the **session's**, while the around-query above it uses the
/// **path's**. They differ only for a caller holding `edit_other_users`, and then the two
/// branches resolve `is_following` against different people.
///
/// # Forwarded
///
/// `collapsedThreadsExtended=true`, for the reason [`get_posts_for_channel`] gives, and any
/// list the metadata pipeline refuses.
#[tracing::instrument(skip_all, fields(user_id = %user_id, channel_id = %channel_id))]
pub async fn get_posts_for_channel_around_last_unread(
    State(state): State<AppState>,
    Path((user_id, channel_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    let if_none_match = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    match serve_posts_around_last_unread(
        &state,
        &user_id,
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

async fn serve_posts_around_last_unread(
    state: &AppState,
    user_id: &str,
    channel_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    if_none_match: Option<String>,
) -> Outcome {
    // `me`, resolved before the validity check (web/context.go:301).
    let user_id = if user_id == "me" {
        session.0.user_id.as_str()
    } else {
        user_id
    };

    // `c.RequireUserId().RequireChannelId()` — user first, the reverse of `getChannelUnread`.
    if !is_valid_id(user_id) {
        return Outcome::Failed(ApiError::invalid_url_param("user_id"));
    }
    if !is_valid_id(channel_id) {
        return Outcome::Failed(ApiError::invalid_url_param("channel_id"));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let channel = match state.app.get_channel(channel_id).await {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };
    let (has_permission, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;
    if !has_permission {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    let limit_after = parse_limit(query, "limit_after");
    let limit_before = parse_limit(query, "limit_before");
    if limit_after == 0 {
        return Outcome::Failed(ApiError::invalid_url_param("limit_after"));
    }

    // `r.URL.Query().Get(...) == "true"` — an exact string compare, not `strconv.ParseBool`, so
    // `?collapsedThreads=1` is **false** here where it would be true on a route using the
    // parser. All three of these are spelled that way.
    let skip_fetch_threads = query_flag_is_literally_true(query, SKIP_FETCH_THREADS_UNREAD_PARAM);
    let collapsed_threads = query_flag_is_literally_true(query, COLLAPSED_THREADS_UNREAD_PARAM);
    if query_flag_is_literally_true(query, COLLAPSED_THREADS_EXTENDED_PARAM) {
        return Outcome::Forward;
    }

    let list = match state
        .app
        .get_posts_for_channel_around_last_unread(
            channel_id,
            user_id,
            limit_before,
            limit_after,
            skip_fetch_threads,
            collapsed_threads,
        )
        .await
    {
        Ok(list) => list,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    // `etag` stays empty on the ordinary path, and the header is only written when it is not.
    let mut etag = String::new();
    let list = if list.order.as_ref().is_none_or(Vec::is_empty) {
        etag = state.app.get_posts_etag(channel_id).await;
        if if_none_match.as_deref() == Some(etag.as_str()) {
            return Outcome::Served(
                (
                    StatusCode::NOT_MODIFIED,
                    [(ETAG.as_str(), etag.as_str()), ("x-mmrs-served-by", "rust")],
                )
                    .into_response(),
            );
        }

        // `app.PageDefault` is 0, and the page size is `limitBefore` — *not* `limitAfter`, and
        // not `PerPageDefault`. The `UserId` is the **session's**, unlike the call above.
        let opts = GetPostsOptions {
            channel_id,
            user_id: &session.0.user_id,
            page: 0,
            per_page: limit_before,
            skip_fetch_threads,
            collapsed_threads,
            include_deleted: false,
        };
        match state.app.get_posts_page(opts).await {
            Ok(list) => list,
            Err(err) => return Outcome::Failed(ApiError::from(err)),
        }
    } else {
        list
    };

    let mut prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // Go's comment: computed **after** filtering, so they only ever name posts in the response.
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
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise PostList");
        return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
            "getPostsForChannelAroundLastUnread",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }

    // The header goes on **only** when the fallback ran. A response carrying unread posts has no
    // `ETag` at all, which is why this route can 304 for a caught-up reader and never otherwise.
    let mut response = (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response();
    if !etag.is_empty()
        && let Ok(value) = etag.parse()
    {
        response.headers_mut().insert(HEADER_ETAG_SERVER, value);
    }

    Outcome::Served(response)
}

/// Port of `getEditHistoryForPost` (api4/post.go:701), reached as
/// `GET /api/v4/posts/{post_id}/edit_history`.
///
/// # Every refusal on this route is `edit_post`, including the ones that are not refusals
///
/// Three separate failures all become `SetPermissionError(PermissionEditPost)`:
///
/// 1. **The post does not exist.** Go does not propagate `GetSinglePost`'s 404 — it *discards*
///    the error and raises a 403. So a post id naming nothing is a permission error here, where
///    the same id is a 404 through `getPost`.
/// 2. The caller lacks `edit_post` on the post's channel.
/// 3. The caller is not the post's **author**, checked after the channel gate.
///
/// A port that let the 404 through would leak the existence of posts this route deliberately
/// refuses to distinguish.
///
/// # The `PostTypeCard` branch is dead here
///
/// Go exempts `custom_card` posts from the authorship check when `FeatureFlags.IntegratedBoards`
/// is on. That flag is **false** at the pinned SHA and unset in this deployment, so the branch
/// cannot fire; the authorship check is unconditional. Ported as the plain check rather than as
/// a config read, for the reason [`mm_app::file::has_permission_to_file_action`] gives about
/// gates whose answer no reachable configuration changes.
///
/// # `edit_post`, not `read_channel_content`
///
/// The channel gate asks for `edit_post` — a permission an ordinary member *does* hold for its
/// own posts, granted through `channel_user`. It is `SessionHasPermissionToChannel`, which takes
/// the channel **id** rather than the channel, so there is no `GetChannel` and no 404 from one.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(postsList)` — a bare JSON **array** with a trailing newline, not a
/// `PostList`. There is no `order`, no `posts` map and no etag.
#[tracing::instrument(skip_all, fields(post_id = %post_id))]
pub async fn get_edit_history_for_post(
    State(state): State<AppState>,
    Path(post_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `c.RequirePostId()` (web/context.go:411).
    if !is_valid_id(&post_id) {
        return Err(ApiError::invalid_url_param("post_id"));
    }

    let permission_error =
        || ApiError::from(make_permission_error(&session.0, &[&PERMISSION_EDIT_POST]));

    // `includeDeleted` is hard-coded false, and the error is **thrown away** — see the doc
    // comment. This is the one place in the port where an app-layer 404 is deliberately
    // swallowed.
    let Ok(original) = state.app.get_single_post(&post_id, false).await else {
        return Err(permission_error());
    };

    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &original.channel_id, &PERMISSION_EDIT_POST)
        .await;
    if !allowed {
        return Err(permission_error());
    }

    // The authorship check, unconditional here — see the doc comment on `PostTypeCard`.
    if session.0.user_id != original.user_id {
        return Err(permission_error());
    }

    let history = state.app.get_edit_history_for_post(&post_id).await?;

    let mut body = serde_json::to_vec(&history).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the edit history");
        ApiError::from(mm_model::utils::AppError::new(
            "getEditHistoryForPost",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// `model.HeaderFirstInaccessiblePostTime`. Go's constant is the literal header name; it is
/// spelled out here because `client4.go`, where the constant lives, is out of scope — and the
/// value was read off the running server, which is the better oracle anyway.
const HEADER_FIRST_INACCESSIBLE_POST_TIME: &str = "First-Inaccessible-Post-Time";

/// `getPostsByIds`'s cap on the id list (api4/post.go:641).
const POSTS_BY_IDS_MAX: usize = 1000;

/// Port of `getPostsByIds` (api4/post.go:631) — `POST /api/v4/posts/ids`.
///
/// The webapp calls this to hydrate permalinks, search hits and thread roots it does not already
/// hold, so the ids in one request routinely span several channels.
///
/// # Three refusals, in Go's order
///
/// 1. **Not a JSON array of strings** → 400 `api.payload.parse.error`.
/// 2. **No ids** (`[]`, and `null`, which `SortedArrayFromJSON` reduces to the same thing) → 400
///    `api.context.invalid_body_param.app_error` naming **`post_ids`**. Unlike its neighbour
///    `getBulkReactions`, this route *has* the length check, so the `IN ()` bug that makes an
///    empty bulk-reactions request a 500 is unreachable here.
/// 3. **More than 1000 ids** → 400 `api.post.posts_by_ids.invalid_body.request_error`. Counted
///    **after** de-duplication, so 1500 copies of one id is a legal request.
///
/// # An all-unknown id list is a 404, not `[]`
///
/// The store raises `ErrNotFound` for zero rows and the app layer turns it into a 404
/// `app.post.get.app_error`. But a list mixing a known id with unknown ones is a 200 carrying
/// only the known post — the miss is not reported. So "some ids were wrong" and "every id was
/// wrong" are answered by different status codes.
///
/// # Filtering is silent, and it happens twice
///
/// A post whose channel `GetChannels` did not return is skipped; so is a post whose channel the
/// session cannot read. Neither produces an error or a placeholder — the post is simply absent
/// from the array. That is what makes the response safe to hand a client that guessed ids, and
/// it is why an unreadable id gives 200 here where `getBulkReactions` gives 403.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(posts)` over a `[]*model.Post` initialised to `[]`, so the empty
/// answer is `[]` (never `null`) and there **is** a trailing newline. Every 200 carries
/// `First-Inaccessible-Post-Time`, which is `0` on any deployment without a Cloud `PostHistory`
/// limit — see [`mm_app::App::get_posts_by_ids`].
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, asked, served))]
pub async fn get_posts_by_ids(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // Split rather than consumed: `PreparePostForClient` can decide a post is beyond this
    // server's reach, and the forward path then needs the body it was given.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return posts_by_ids_parse_error().into_response();
        }
    };

    match serve_posts_by_ids(&state, &session, &bytes).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => {
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// `model.NewAppError("getPostsByIds", model.PayloadParseError, nil, "", 400)`.
fn posts_by_ids_parse_error() -> ApiError {
    ApiError::from(mm_model::utils::AppError::new(
        "getPostsByIds",
        PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
}

/// The validation half of `getPostsByIds` (api4/post.go:632-644), in Go's order.
///
/// 1. **Not a JSON array of strings** → 400 `api.payload.parse.error`. The decoder's habits —
///    trailing bytes ignored, a `null` element read as `""`, a `null` body read as an empty
///    list — live in [`sorted_array_from_json`] and its oracle.
/// 2. **No ids** → 400 `api.context.invalid_body_param.app_error` naming **`post_ids`**, plural.
///    The name reaches the wire only through i18n, which this port does not do (our `message`
///    is the raw id, [D-092]) — so it is pinned by the unit test below rather than by the parity
///    suite, which cannot see it.
/// 3. **More than 1000** → 400 `api.post.posts_by_ids.invalid_body.request_error` carrying
///    `MaxLength`. Counted **after** `SortedArrayFromJSON` de-duplicates, so 1500 copies of one
///    id is a legal request for one post.
#[allow(clippy::result_large_err)]
fn parse_post_ids(body: &[u8]) -> Result<Vec<String>, ApiError> {
    let post_ids = sorted_array_from_json(body).map_err(|err| {
        tracing::debug!(error = %err, "post_ids body did not decode");
        posts_by_ids_parse_error()
    })?;

    if post_ids.is_empty() {
        return Err(ApiError::invalid_param("post_ids"));
    }

    if post_ids.len() > POSTS_BY_IDS_MAX {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "MaxLength".to_owned(),
            serde_json::Value::from(POSTS_BY_IDS_MAX),
        );
        return Err(ApiError::from(mm_model::utils::AppError::new(
            "getPostsByIds",
            "api.post.posts_by_ids.invalid_body.request_error",
            Some(params),
            String::new(),
            400,
        )));
    }

    Ok(post_ids)
}

async fn serve_posts_by_ids(
    state: &AppState,
    session: &AuthenticatedSession,
    bytes: &[u8],
) -> Outcome {
    let post_ids = match parse_post_ids(bytes) {
        Ok(ids) => ids,
        Err(err) => return Outcome::Failed(err),
    };
    tracing::Span::current().record("asked", post_ids.len());

    let (found, first_inaccessible_post_time) = match state.app.get_posts_by_ids(&post_ids).await {
        Ok(found) => found,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    // Go collects every post's channel id, duplicates included, and hands the lot to
    // `GetChannels`; the store's `IN` collapses them. Collected the same way rather than
    // de-duplicated first, so the store sees what Go's store sees.
    let channel_ids: Vec<String> = found.iter().map(|post| post.channel_id.clone()).collect();
    let channels = match state.app.get_channels(&channel_ids).await {
        Ok(channels) => channels,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };
    let channels_by_id: std::collections::HashMap<&str, &mm_model::channel::Channel> =
        channels.iter().map(|c| (c.id.as_str(), c)).collect();

    // `&model.PreparePostForClientOpts{IncludePriority: true}` — the same options `getPost`
    // passes, and every other field false.
    let opts = PreparePostForClientOpts {
        include_priority: true,
        ..PreparePostForClientOpts::default()
    };

    let mut posts: Vec<mm_model::post::Post> = Vec::new();
    for post in &found {
        // Go's `channelMap[post.ChannelId]` miss is a bare `continue`: a post whose channel
        // `GetMany` filtered out — a board, a space, or a hard-deleted row — is dropped without
        // comment rather than being an error.
        let Some(channel) = channels_by_id.get(post.channel_id.as_str()) else {
            continue;
        };

        // `isMemberForAllPosts` is computed from the second return value and used only to tag
        // the audit record, which is not ported ([D-028]).
        let (has_permission, _is_member) = state
            .app
            .session_has_permission_to_read_channel(&session.0, channel)
            .await;
        if !has_permission {
            continue;
        }

        let mut prepared = match state
            .app
            .prepare_post_for_client_with_embeds_and_images(post, opts)
            .await
        {
            Ok(prepared) => prepared,
            Err(PrepareError::Unreproducible(reason)) => {
                tracing::debug!(reason, post_id = %post.id, "forwarding to Go");
                return Outcome::Forward;
            }
            Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
        };

        // `post.StripActionIntegrations()`, in place and before the slice is encoded. Note that
        // Go does **not** call `SanitizePostMetadataForUser` here, unlike `getPost` — a preview
        // embed the caller cannot see is left in place on this route.
        //
        // **Unreachable today**, and deliberately kept: the only posts with integrations to
        // strip carry an `attachments` prop, which is in `mm_app::post::REFUSED_PROPS`, so
        // `prepare_post_for_client_with_embeds_and_images` has already forwarded them. Narrowing
        // that refusal set without this line would start leaking `integration` blocks — url,
        // and the context map — to every client.
        prepared.strip_action_integrations();
        posts.push(prepared);
    }
    tracing::Span::current().record("served", posts.len());

    // `json.NewEncoder(w).Encode` — Go escapes `<`, `>` and `&`, which a post message or a props
    // value can carry freely, and appends the newline `json.Marshal` does not.
    let encoded = match go_json_marshal(&posts) {
        Ok(encoded) => encoded,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the posts");
            return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
                "getPostsByIds",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )));
        }
    };
    let mut body = encoded.into_bytes();
    body.push(b'\n');

    Outcome::Served(
        (
            StatusCode::OK,
            [
                (
                    HEADER_FIRST_INACCESSIBLE_POST_TIME,
                    first_inaccessible_post_time.to_string().as_str(),
                ),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

/// Port of `getFlaggedPostsForUser` (api4/post.go:475) —
/// `GET /api/v4/users/{user_id}/posts/flagged`.
///
/// The webapp's "Saved messages" panel. Three query shapes, chosen in this order: `channel_id`
/// wins if present, then `team_id`, then neither — so passing both filters by channel and
/// ignores the team.
///
/// # `page` is an offset, not a page
///
/// `c.Params.Page` is handed straight to the store's `offset` (api4/post.go:493) with no
/// multiplication by `per_page`. `?page=1&per_page=1` skips **one post**; `?page=2` skips two.
/// Measured against the running server. See [`mm_store::PostStore::get_flagged_posts`], which
/// also documents the missing parentheses in Go's team filter.
///
/// # The permission gate is `edit_other_users`, and it runs before anything else
///
/// `SessionHasPermissionToUser` — self, or a system admin. The refusal names
/// `PermissionEditOtherUsers`, which is a *write* permission guarding a read, and it is what
/// Go reports whatever the real reason.
///
/// # Then a second, per-channel gate, and it is silent
///
/// Each post's channel is looked up and `SessionHasPermissionToReadChannel`'d; a post whose
/// channel is missing from that lookup, or unreadable, is `continue`d past. So this route can
/// answer 200 with an empty list where the caller genuinely has flags — the same silent
/// filtering as [`get_posts_by_ids`]. The permission answer is **cached per channel id** for the
/// duration of the request, which is only a performance detail here because the answer cannot
/// change mid-list.
///
/// # Wire format
///
/// `clientPostList.EncodeJSON(w)` — a `PostList` with a trailing newline, and `NewPostList`
/// gives it a non-nil `order` and `posts`, so an empty answer is `{"order":[],"posts":{},…}`
/// rather than nulls.
#[tracing::instrument(skip_all, fields(user_id = %user_id, kept))]
pub async fn get_flagged_posts_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);

    match serve_flagged_posts(&state, &user_id, &session, query.as_deref()).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

async fn serve_flagged_posts(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
) -> Outcome {
    // `c.RequireUserId()` (web/context.go:397).
    if !is_valid_id(user_id) {
        return Outcome::Failed(ApiError::invalid_url_param("user_id"));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    // Go reads both, then branches on `channelId != ""` first. A blank `?channel_id=` is the
    // same as absent, because it compares against the empty string rather than testing presence.
    let channel_id = query_first(query, "channel_id").unwrap_or_default();
    let team_id = query_first(query, "team_id").unwrap_or_default();
    let (channel_filter, team_filter) = if !channel_id.is_empty() {
        (channel_id.as_str(), "")
    } else {
        ("", team_id.as_str())
    };

    // `c.Params.Page` into the store's `offset` — see the doc comment above.
    let list = match state
        .app
        .get_flagged_posts(
            user_id,
            channel_filter,
            team_filter,
            parse_page(query),
            parse_per_page(query),
        )
        .await
    {
        Ok(list) => list,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    // Borrowed, not cloned: the whole point of the loop below is to copy the few posts that
    // survive the gate, and cloning the map first would copy the ones that do not.
    let no_posts = mm_model::post_list::PostMap::new();
    let posts = list.posts.as_ref().unwrap_or(&no_posts);
    let channel_ids: Vec<String> = list
        .order
        .iter()
        .flatten()
        .filter_map(|id| posts.get(id))
        .map(|post| post.channel_id.clone())
        .collect();
    // **Skipped when there is nothing to look up.** `SqlChannelStore.GetMany` raises
    // `ErrNotFound` for zero rows, which [`mm_app::App::get_channels`] turns into a 404 — and a
    // user with no visible flagged posts reaches here with an empty id list. Go answers that
    // request `200 {"order":[],"posts":{},…}`, measured; whatever squirrel does with an empty
    // `IN`, the channel map cannot be observed when no post survives to consult it. Calling the
    // store anyway would turn Go's empty list into our 404, which is what the first run did.
    let channels = if channel_ids.is_empty() {
        Vec::new()
    } else {
        match state.app.get_channels(&channel_ids).await {
            Ok(channels) => channels,
            Err(err) => return Outcome::Failed(ApiError::from(err)),
        }
    };
    let channels_by_id: std::collections::HashMap<&str, &mm_model::channel::Channel> =
        channels.iter().map(|c| (c.id.as_str(), c)).collect();

    // Go rebuilds the list from scratch rather than filtering in place, so a post whose channel
    // is unreadable leaves neither an `order` entry nor a `posts` key behind.
    let mut kept = mm_model::post_list::PostList::new();
    let mut channel_read_permission: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for id in list.order.iter().flatten() {
        let Some(post) = posts.get(id) else {
            continue;
        };
        let allowed = match channel_read_permission.get(&post.channel_id) {
            Some(allowed) => *allowed,
            None => {
                let allowed = match channels_by_id.get(post.channel_id.as_str()) {
                    // Go's `channelMap` miss is a bare `continue` that never writes the cache —
                    // so a missing channel is re-looked-up for every post that names it. Same
                    // answer either way; the cache write is skipped here for the same reason.
                    None => continue,
                    Some(channel) => {
                        let (has_permission, _is_member) = state
                            .app
                            .session_has_permission_to_read_channel(&session.0, channel)
                            .await;
                        has_permission
                    }
                };
                channel_read_permission.insert(post.channel_id.clone(), allowed);
                allowed
            }
        };
        if !allowed {
            continue;
        }
        kept.add_post(post.clone());
        kept.add_order(id.clone());
    }

    // `pl.SortByCreateAt()` — the store already ordered by `CreateAt DESC`, and this sorts the
    // same way, but it is Go's and a filtered list is not obliged to have kept the order.
    kept.sort_by_create_at();
    tracing::Span::current().record("kept", kept.order.as_ref().map_or(0, Vec::len));

    let prepared = match state.app.prepare_post_list_for_client(&kept).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, user_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let (mut sanitized, _all_previews_have_membership) = match state
        .app
        .sanitize_post_list_metadata_for_user(prepared, &session.0.user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, user_id, "forwarding to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise the flagged PostList");
        return Outcome::Failed(ApiError::from(mm_model::utils::AppError::new(
            "getFlaggedPostsForUser",
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
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

#[cfg(test)]
mod posts_by_ids_tests {
    use super::*;

    fn error_of(body: &str) -> Box<mm_model::utils::AppError> {
        parse_post_ids(body.as_bytes())
            .expect_err("this body must be rejected")
            .0
    }

    /// The i18n parameters never reach the wire on this deployment, so the parity suite cannot
    /// tell `post_ids` from `post_id` — and a mutation swapping them survived until this test
    /// existed. `Name` is what the translated message interpolates, so it is wire format the
    /// day the bundle lands.
    #[test]
    fn the_empty_list_names_post_ids_plural() {
        for body in ["[]", "null"] {
            let err = error_of(body);
            assert_eq!(err.id, "api.context.invalid_body_param.app_error", "{body}");
            assert_eq!(err.status_code, 400, "{body}");
            assert_eq!(
                err.params.as_ref().and_then(|p| p.get("Name")),
                Some(&serde_json::Value::from("post_ids")),
                "{body}: plural, and it is the body parameter's name not the URL one"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_an_array_of_strings_is_a_parse_error() {
        for body in ["{}", "[1,2]", "not json", "\"a\""] {
            let err = error_of(body);
            assert_eq!(err.id, PAYLOAD_PARSE_ERROR, "{body}");
            assert_eq!(err.status_code, 400, "{body}");
        }
    }

    /// The cap is a strict `>`, and it is applied to the **de-duplicated** list.
    #[test]
    fn the_cap_is_one_thousand_distinct_ids() {
        let distinct: Vec<String> = (0..=POSTS_BY_IDS_MAX).map(|i| format!("{i:026}")).collect();
        let over = serde_json::to_string(&distinct).expect("serialises");
        let err = error_of(&over);
        assert_eq!(err.id, "api.post.posts_by_ids.invalid_body.request_error");
        assert_eq!(
            err.params.as_ref().and_then(|p| p.get("MaxLength")),
            Some(&serde_json::Value::from(POSTS_BY_IDS_MAX))
        );

        let exactly = serde_json::to_string(&distinct[..POSTS_BY_IDS_MAX]).expect("serialises");
        assert_eq!(
            parse_post_ids(exactly.as_bytes())
                .expect("1000 is under the cap")
                .len(),
            POSTS_BY_IDS_MAX
        );

        let duplicated =
            serde_json::to_string(&vec!["a"; POSTS_BY_IDS_MAX + 500]).expect("serialises");
        assert_eq!(
            parse_post_ids(duplicated.as_bytes())
                .expect("de-duplicated before the count")
                .len(),
            1
        );
    }
}
