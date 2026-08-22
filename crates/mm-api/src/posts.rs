//! Port of `api4/post.go`'s `getPost` — `GET /api/v4/posts/{post_id}`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::response::{IntoResponse, Response};
use mm_app::post::{PrepareError, PreparePostForClientOpts};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::utils::is_valid_id;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::query_flag_is_true;
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
