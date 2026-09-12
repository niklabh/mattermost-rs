//! Port of `channels/api4/channel_join_request.go` — the whole file, all seven routes.
//!
//! ```text
//! POST   /api/v4/channels/{channel_id}/join_request               requestJoinChannel
//! GET    /api/v4/channels/{channel_id}/join_request               getMyChannelJoinRequest
//! DELETE /api/v4/channels/{channel_id}/join_request               withdrawMyChannelJoinRequest
//! GET    /api/v4/channels/{channel_id}/join_requests              getChannelJoinRequests
//! GET    /api/v4/channels/{channel_id}/join_requests/count        countPendingChannelJoinRequests
//! PATCH  /api/v4/channels/{channel_id}/join_requests/{request_id} patchChannelJoinRequest
//! GET    /api/v4/users/{user_id}/channel_join_requests            getMyChannelJoinRequests
//! ```
//!
//! # These routes do not exist on the deployment this server fronts, and that is the first fact
//!
//! `initChannelJoinRequestRoutes` (channel_join_request.go:18) opens with
//!
//! ```go
//! if !api.srv.Config().FeatureFlags.DiscoverableChannels { return }
//! ```
//!
//! `DiscoverableChannels` is **false** at the pinned SHA (`feature_flags.go:208`) and nothing in
//! this deployment sets it, so gorilla/mux has never heard of `/channels/{id}/join_request` and
//! the answer is not a 501 or a 403 but the mux's own **404 `api.context.404.app_error`** —
//! measured against the stack's Go server, not inferred. [D-153] is the ledger entry for that pin.
//!
//! That shapes the port exactly as it shaped [`crate::views`]: the flag is consulted as the
//! **first statement of every handler**, and when it is off the request is *forwarded* rather than
//! refused locally. Go then writes its own 404, URL interpolation, `request_id` and all, and there
//! is nothing here to keep in step.
//!
//! There is a second gate inside each Go handler — `requireDiscoverableChannelsEnabled`, which
//! answers `api.channel.discoverable_join_request.feature_disabled.app_error` at 404. It is
//! **unreachable**: the only way to reach a handler is through a route the same flag registered,
//! so the id exists and no request produces it. Named here so a reader looking for it knows it was
//! read and not missed.
//!
//! # Verifying the served shape needed a third Go server
//!
//! With the flag off there is no oracle — every route is a 404 and a comparison proves only that
//! two servers agree about a route neither serves. `scripts/go-discoverable.sh` starts a pinned Go
//! process on `MMRS_GO_PORT + 31` with `MM_FEATUREFLAGS_DISCOVERABLECHANNELS=true`, sharing the
//! database, the configuration document and the `Sessions` table. Turning the flag on in
//! `scripts/go-server.sh` would have been wrong for the same reason `go-boards.sh` exists: the
//! flag also changes `getChannel`, `createChannel` and `patchChannel`, which the parity suite
//! already asserts against. Every "measured" in this file means measured against that process.
//!
//! # Five details the wire depends on
//!
//! - **Every success body carries a trailing newline** (`json.NewEncoder(w).Encode`), including
//!   the two synthetic maps — `{"status":"approved"}` and `{"count":N}`.
//! - **`requestJoinChannel` is a 201**, on both of its arms, and the others are 200.
//! - **`getMyChannelJoinRequest`'s miss is a bodiless 404** — `w.WriteHeader(404)` and nothing
//!   else, deliberately *not* the `null` the app layer returns. Zero bytes, no `Content-Type`.
//! - **An empty list is `{"requests":null,…}`**, never `[]`. Nothing normalises the store's nil
//!   slice.
//! - **`getMyChannelJoinRequests` refuses another user with `edit_other_users`** — a permission
//!   it never checks and could not satisfy. `c.Params.UserId != session.UserId` is the whole test,
//!   so a system admin is refused their own colleague's list.
//!
//! # The two list routes read `status` from the query and validate nothing
//!
//! `?status=bogus` is silently rewritten to `pending` by `sanitizeJoinRequestListOpts`, not
//! refused — so a client typo lists the wrong thing with a 200. Measured.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_join_request::JoinRequestOutcome;
use mm_app::channel_member::MemberWrite;
use mm_model::channel_join_request::{
    CHANNEL_JOIN_REQUEST_STATUS_APPROVED, ChannelJoinRequestPatch, GetChannelJoinRequestsOpts,
};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_CHANNEL_JOIN_REQUESTS, make_permission_error,
};
use mm_model::utils::{AppError, decode_one_from_json, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, resolve_me};
use crate::error::ApiError;
use crate::proxy;

/// `channelJoinRequestBody` (channel_join_request.go:36) — one field, and **no `omitempty`**, so a
/// body of `{}` decodes to an empty message rather than failing.
#[derive(Debug, Default, serde::Deserialize)]
struct ChannelJoinRequestBody {
    #[serde(default, rename = "message")]
    message: String,
}

/// What a handler decided, before it has been turned into bytes.
enum Outcome {
    Served(Response),
    Failed(ApiError),
    /// Hand the whole request to Go — the feature is off here, or a membership write this server
    /// cannot reproduce sits on the approve path.
    Forward,
}

impl Outcome {
    async fn finish(self, state: AppState, request: Request) -> Response {
        match self {
            Outcome::Served(response) => response,
            Outcome::Failed(err) => err.into_response(),
            Outcome::Forward => proxy::forward_to_go(State(state), request).await,
        }
    }
}

/// The byte `json.NewEncoder(w).Encode` appends after every document.
///
/// Named rather than inlined for the reason `views.rs` gives: a mutation plan converts `\n` in a
/// pattern into a real newline, so no anchor can quote a literal `b'\n'`.
const ENCODER_NEWLINE: u8 = b'\n';

/// `json.NewEncoder(w).Encode(value)` — a **trailing newline**, which a plain `to_vec` omits.
fn encoded(handler: &'static str, status: StatusCode, value: &impl serde::Serialize) -> Outcome {
    match serde_json::to_vec(value) {
        Ok(mut body) => {
            body.push(ENCODER_NEWLINE);
            Outcome::Served(
                (
                    status,
                    [
                        ("Content-Type", "application/json"),
                        ("x-mmrs-served-by", "rust"),
                    ],
                    body,
                )
                    .into_response(),
            )
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the join-request response");
            Outcome::Failed(ApiError::from(AppError::new(
                handler,
                "api.marshal_error",
                None,
                String::new(),
                500,
            )))
        }
    }
}

/// `c.RequireChannelId()` / `c.RequireUserId()` (web/context.go:388, :296) — a 400 naming the
/// parameter.
fn require_id(value: &str, parameter: &'static str) -> Result<(), ApiError> {
    if is_valid_id(value) {
        return Ok(());
    }
    Err(ApiError::invalid_url_param(parameter))
}

/// The `PermissionManageChannelJoinRequests` gate the three admin routes share.
///
/// `SessionHasPermissionToChannel` — **membership-based**, with no open-channel fallback, so a
/// non-member of a channel gets the same 403 as a member without the right. The refusal names the
/// permission it checked.
async fn require_manage(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
) -> Result<(), ApiError> {
    let (allowed, _) = state
        .app
        .session_has_permission_to_channel(
            &session.0,
            channel_id,
            &PERMISSION_MANAGE_CHANNEL_JOIN_REQUESTS,
        )
        .await;
    if allowed {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        &session.0,
        &[&PERMISSION_MANAGE_CHANNEL_JOIN_REQUESTS],
    )))
}

/// `model.GetChannelJoinRequestsOpts{Status: query, Page: c.Params.Page, PerPage: c.Params.PerPage}`.
///
/// The two numbers come from `web.ParamsFromRequest`'s clamps (`page < 0 → 0`, `per_page > 200 →
/// 200`, a parse failure → 60) and are then clamped **again** by `sanitizeJoinRequestListOpts`.
/// Both layers exist in Go; only the second can turn a `per_page` of 0 into 60, because the first
/// lets zero through.
fn list_opts(query: Option<&str>) -> GetChannelJoinRequestsOpts {
    GetChannelJoinRequestsOpts {
        status: crate::channels::query_first(query, "status")
            .unwrap_or_default()
            .to_string(),
        page: parse_page(query),
        per_page: parse_per_page(query),
    }
}

// ---------------------------------------------------------------------------------------------
// requestJoinChannel
// ---------------------------------------------------------------------------------------------

/// Port of `requestJoinChannel` (channel_join_request.go:46) — `POST
/// /api/v4/channels/{channel_id}/join_request`.
///
/// # The body is decoded before anything is looked up
///
/// `RequireChannelId`, the feature gate, then `json.NewDecoder(r.Body).Decode(&body)`. So a POST
/// to a channel that does not exist with a broken body is `invalid_body_param` naming **`body`**,
/// not a 404 — and an *empty* body is the same 400, because `Decode` on zero bytes is `io.EOF`.
/// Measured.
///
/// # Both arms are 201 and they carry different shapes
///
/// The ABAC fast path writes `{"status":"approved"}` and the ordinary path writes the request
/// itself. The fast path is dark on this deployment — see
/// [`mm_app::App::channel_access_controlled`] — so only the second is reachable, and the constant
/// is [`CHANNEL_JOIN_REQUEST_STATUS_APPROVED`] rather than a literal so the two cannot drift.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn request_join_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the join-request body");
            return ApiError::invalid_param("body").into_response();
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(body.clone()));

    serve_request_join(&state, &channel_id, &session, &body)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_request_join(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
    body: &[u8],
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }

    let body: ChannelJoinRequestBody = match decode_one_from_json(body) {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "malformed join-request body");
            return Outcome::Failed(ApiError::invalid_param("body"));
        }
    };

    match state
        .app
        .request_join_channel(&session.0.user_id, channel_id, &body.message)
        .await
    {
        Ok(JoinRequestOutcome::Joined) => encoded(
            "requestJoinChannel",
            StatusCode::CREATED,
            &serde_json::json!({ "status": CHANNEL_JOIN_REQUEST_STATUS_APPROVED }),
        ),
        Ok(JoinRequestOutcome::Pending(req)) => {
            encoded("requestJoinChannel", StatusCode::CREATED, &req)
        }
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// getMyChannelJoinRequest
// ---------------------------------------------------------------------------------------------

/// Port of `getMyChannelJoinRequest` (channel_join_request.go:92) — `GET
/// /api/v4/channels/{channel_id}/join_request`.
///
/// **A miss is a bodiless 404.** `w.WriteHeader(http.StatusNotFound)` and `return` — no
/// `AppError`, no `Content-Type`, zero bytes. Go's comment says why: clients must be able to tell
/// "no pending request" from "service down". Answering the usual `api.context.404` body here
/// would be a different response to every client that reads `id`.
///
/// The channel is never loaded, so this answers 404 for a channel that does not exist, one the
/// caller cannot see, and one with no request — all identically.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn get_my_channel_join_request(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }

    serve_get_my(&state, &channel_id, &session)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_get_my(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }

    match state
        .app
        .get_my_channel_join_request(&session.0.user_id, channel_id)
        .await
    {
        Ok(Some(req)) => encoded("getMyChannelJoinRequest", StatusCode::OK, &req),
        Ok(None) => Outcome::Served(
            (
                StatusCode::NOT_FOUND,
                [("x-mmrs-served-by", "rust")],
                // Deliberately empty: Go writes the header and nothing else.
                Vec::<u8>::new(),
            )
                .into_response(),
        ),
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// withdrawMyChannelJoinRequest
// ---------------------------------------------------------------------------------------------

/// Port of `withdrawMyChannelJoinRequest` (channel_join_request.go:117) — `DELETE
/// /api/v4/channels/{channel_id}/join_request`.
///
/// # It looks the request up **twice**, by two different keys
///
/// `GetMyChannelJoinRequest` (channel + user, pending only) to find the id, then
/// `WithdrawChannelJoinRequest` (by id) which re-reads it and re-checks the owner and the status.
/// The handler's own nil check raises `app.channel.join_request.not_found.app_error` at 404 —
/// the same id and status the app layer's own miss raises, so the duplication is invisible from
/// outside and the second read's owner check is unreachable through this route.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn withdraw_my_channel_join_request(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }

    serve_withdraw(&state, &channel_id, &session)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_withdraw(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }

    let existing = match state
        .app
        .get_my_channel_join_request(&session.0.user_id, channel_id)
        .await
    {
        Ok(Some(req)) => req,
        Ok(None) => {
            return Outcome::Failed(ApiError::from(AppError::new(
                "withdrawMyChannelJoinRequest",
                "app.channel.join_request.not_found.app_error",
                None,
                format!("channel_id={channel_id}"),
                404,
            )));
        }
        Err(err) => return Outcome::Failed(err.into()),
    };

    match state
        .app
        .withdraw_channel_join_request(&existing.id, &session.0.user_id)
        .await
    {
        Ok(updated) => encoded("withdrawMyChannelJoinRequest", StatusCode::OK, &updated),
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// getChannelJoinRequests
// ---------------------------------------------------------------------------------------------

/// Port of `getChannelJoinRequests` (channel_join_request.go:158) — `GET
/// /api/v4/channels/{channel_id}/join_requests`, the admin queue.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn get_channel_join_requests(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }
    let query = request.uri().query().map(str::to_owned);

    serve_channel_list(&state, &channel_id, &session, query.as_deref())
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_channel_list(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }
    if let Err(err) = require_manage(state, session, channel_id).await {
        return Outcome::Failed(err);
    }

    match state
        .app
        .get_channel_join_requests(channel_id, list_opts(query))
        .await
    {
        Ok(list) => encoded("getChannelJoinRequests", StatusCode::OK, &list),
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// countPendingChannelJoinRequests
// ---------------------------------------------------------------------------------------------

/// Port of `countPendingChannelJoinRequests` (channel_join_request.go:186) — `GET
/// /api/v4/channels/{channel_id}/join_requests/count`, the channel-header badge.
///
/// The body is `map[string]int64{"count": n}` — an object, not a bare number, and the value is
/// **not** affected by `?status=`: this route counts pending and nothing else.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn count_pending_channel_join_requests(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }

    serve_count(&state, &channel_id, &session)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_count(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }
    if let Err(err) = require_manage(state, session, channel_id).await {
        return Outcome::Failed(err);
    }

    match state
        .app
        .count_pending_channel_join_requests(channel_id)
        .await
    {
        Ok(count) => encoded(
            "countPendingChannelJoinRequests",
            StatusCode::OK,
            &serde_json::json!({ "count": count }),
        ),
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// patchChannelJoinRequest
// ---------------------------------------------------------------------------------------------

/// Port of `patchChannelJoinRequest` (channel_join_request.go:213) — `PATCH
/// /api/v4/channels/{channel_id}/join_requests/{request_id}`, the review.
///
/// # The order, which decides which refusal a bad request gets
///
/// 1. `RequireChannelId` — 400 `channel_id`.
/// 2. The feature gate.
/// 3. **`IsValidId(request_id)`** — an explicit `SetInvalidURLParam("request_id")`, *not*
///    `RequireRequestId`; a 26-character-shaped id is the only thing that passes. Note this runs
///    **before** the permission check, so a malformed id is a 400 even for a caller with no rights
///    at all.
/// 4. The `manage_channel_join_requests` gate — so reviewing against a channel the caller cannot
///    manage is a **403**, even when the request id belongs to a different channel entirely.
///    Measured: the cross-channel 404 in the app layer is only reachable by an admin of *both*.
/// 5. Decode the patch — 400 naming `channel_join_request_patch`, a different parameter name from
///    `requestJoinChannel`'s `body`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, request_id = %request_id, user_id = %session.0.user_id))]
pub async fn patch_channel_join_request(
    State(state): State<AppState>,
    Path((channel_id, request_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the join-request patch body");
            return ApiError::invalid_param("channel_join_request_patch").into_response();
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(body.clone()));

    serve_patch(&state, &channel_id, &request_id, &session, &body)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_patch(
    state: &AppState,
    channel_id: &str,
    request_id: &str,
    session: &AuthenticatedSession,
    body: &[u8],
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }
    if !is_valid_id(request_id) {
        return Outcome::Failed(ApiError::invalid_url_param("request_id"));
    }
    if let Err(err) = require_manage(state, session, channel_id).await {
        return Outcome::Failed(err);
    }

    let patch: ChannelJoinRequestPatch = match decode_one_from_json(body) {
        Ok(patch) => patch,
        Err(err) => {
            tracing::debug!(error = %err, "malformed join-request patch");
            return Outcome::Failed(ApiError::invalid_param("channel_join_request_patch"));
        }
    };

    match state
        .app
        .update_channel_join_request(request_id, channel_id, &patch, &session.0.user_id)
        .await
    {
        Ok(MemberWrite::Done(updated)) => {
            encoded("patchChannelJoinRequest", StatusCode::OK, &updated)
        }
        Ok(MemberWrite::Forward(why)) => {
            tracing::debug!(why, "forwarding an approval this server cannot reproduce");
            Outcome::Forward
        }
        Err(err) => Outcome::Failed(err.into()),
    }
}

// ---------------------------------------------------------------------------------------------
// getMyChannelJoinRequests
// ---------------------------------------------------------------------------------------------

/// Port of `getMyChannelJoinRequests` (channel_join_request.go:270) — `GET
/// /api/v4/users/{user_id}/channel_join_requests`.
///
/// # `edit_other_users` is reported and never checked
///
/// The gate is `c.Params.UserId != c.AppContext.Session().UserId`, a plain string comparison, and
/// the refusal is `SetPermissionError(PermissionEditOtherUsers)`. So a system admin holding that
/// permission is refused another user's list all the same — the error names a right the check
/// never consults. `me` resolves to the session user first (`RequireUserId`), so
/// `/users/me/channel_join_requests` is always the caller's own.
#[tracing::instrument(skip_all, fields(user_id = %user_id, session_user_id = %session.0.user_id))]
pub async fn get_my_channel_join_requests(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_discoverable_channels {
        return proxy::forward_to_go(State(state), request).await;
    }
    let query = request.uri().query().map(str::to_owned);

    serve_my_list(&state, &user_id, &session, query.as_deref())
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_my_list(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
) -> Outcome {
    let user_id = resolve_me(user_id, session);
    if let Err(err) = require_id(user_id, "user_id") {
        return Outcome::Failed(err);
    }

    if user_id != session.0.user_id {
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    // Go passes `session.UserId`, not `c.Params.UserId` — identical by the check above, and the
    // session's is the one that cannot have come from the URL.
    match state
        .app
        .get_my_channel_join_requests(&session.0.user_id, list_opts(query))
        .await
    {
        Ok(list) => encoded("getMyChannelJoinRequests", StatusCode::OK, &list),
        Err(err) => Outcome::Failed(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_status_is_the_empty_string_and_the_app_layer_defaults_it() {
        let opts = list_opts(None);
        assert_eq!(opts.status, "");
        assert_eq!(opts.page, 0);
        assert_eq!(opts.per_page, 60);
    }

    #[test]
    fn the_status_query_parameter_is_taken_verbatim() {
        assert_eq!(list_opts(Some("status=denied")).status, "denied");
        // No validation here — `sanitizeJoinRequestListOpts` rewrites it to `pending`.
        assert_eq!(list_opts(Some("status=bogus")).status, "bogus");
    }

    #[test]
    fn the_pagination_clamps_are_gos_and_zero_survives_this_layer() {
        // `web.ParamsFromRequest` lets a zero `per_page` through; only the app layer turns it
        // into 60. Clamping it here would hide that second clamp entirely.
        assert_eq!(list_opts(Some("per_page=0")).per_page, 0);
        assert_eq!(list_opts(Some("per_page=500")).per_page, 200);
        assert_eq!(list_opts(Some("page=-2")).page, 0);
        assert_eq!(list_opts(Some("page=3&per_page=7")).page, 3);
    }

    #[test]
    fn an_empty_body_decodes_to_an_empty_message_only_when_it_is_an_object() {
        let body: ChannelJoinRequestBody = decode_one_from_json(b"{}").expect("an object decodes");
        assert_eq!(body.message, "");
        // Zero bytes is `io.EOF` in Go and an error here — the 400 both servers answer.
        assert!(decode_one_from_json::<ChannelJoinRequestBody>(b"").is_err());
    }
}
