//! Port of the five `RemoteClusterTokenRequired` routes of `api4/remote_cluster.go`:
//!
//! ```text
//! POST /api/v4/remotecluster/ping                       remoteClusterPing
//! POST /api/v4/remotecluster/msg                        remoteClusterAcceptMessage
//! POST /api/v4/remotecluster/confirm_invite             remoteClusterConfirmInvite
//! POST /api/v4/remotecluster/upload/{upload_id}         uploadRemoteData
//! POST /api/v4/remotecluster/{user_id}/image            remoteSetProfileImage
//! ```
//!
//! The other seven `/remotecluster` routes (the `APISessionRequired` CRUD) are another family's,
//! in [`crate::connected_workspaces`]; these five are the ones a *remote server* calls, gated by
//! [`RemoteClusterTokenRequired`](web/handlers.go:91) rather than by a user session.
//!
//! # Without a licence every one of them is a 401, before any handler code runs
//!
//! The gate is `(*Context).RemoteClusterTokenRequired` (web/context.go:159):
//!
//! ```go
//! if license := c.App.Channels().License(); license == nil ||
//!     !license.HasRemoteClusterService() ||
//!     c.AppContext.Session().Props[model.SessionPropType] != model.SessionTypeRemoteclusterToken {
//!     c.Err = model.NewAppError("", "api.context.session_expired.app_error", nil, "TokenRequired", 401)
//! }
//! ```
//!
//! The stack's Go server carries **no licence** (`go-server.sh` builds without
//! `BuildEnterpriseReady`, so `LoadLicense` never runs — the reasoning `go-licensed.sh` gives),
//! so `License()` is nil and the first disjunct fires for **every** request: the body, the
//! `X-RemoteCluster-Token`/`X-RemoteCluster-Id` headers and the URL parameters are never read,
//! because the gate is upstream of `RequireUploadId`, `RequireUserId` and the handlers'
//! `GetRemoteClusterService` (which would otherwise be the 501 the CRUD family answers). Measured
//! against the stack: all five are `api.context.session_expired.app_error` at 401, token headers
//! or not.
//!
//! # With a licence: the remote-cluster session, served
//!
//! When a licence **with** the remote-cluster service is present the gate's first two disjuncts
//! pass and the answer turns on the session `ServeHTTP` built (web/handlers.go:267-325). That is
//! ported here as [`remote_cluster_session`], in Go's order:
//!
//! | # | the request carries | answer |
//! |---|---|---|
//! | 1 | a cookie, `Authorization` or `?access_token=` | that session (or none) — never `RemoteClusterToken`, so the gate's 401 |
//! | 2 | `X-Cloud-Token` | under a cloud licence `GetCloudSession` (a wrong key is its own 401), else nothing — the gate's 401 either way when it matches |
//! | 3 | `X-RemoteCluster-Token` and no `X-RemoteCluster-Id` | 401 `api.context.remote_id_missing.app_error` |
//! | 4 | both, and `GetRemoteClusterSession` refuses | 401 `api.context.invalid_token.error` |
//! | 5 | both, and the row's `Token` matches | a `RemoteClusterToken` session — past the gate |
//!
//! Row 4 covers a wrong token, an unknown or **deleted** remote, and a row with a NULL column
//! (`mm_store::remote_cluster_store`), all measured on the licensed oracle. The session is a
//! table read and a compare; it needs no `RemoteClusterService`.
//!
//! # What is forwarded past the gate
//!
//! * **`ping`, `msg`, `confirm_invite`, `upload/{upload_id}`**: each body begins with
//!   `GetRemoteClusterService()` and then drives the service (`ReceiveIncomingMsg`,
//!   `ReceiveInviteConfirmation`, the upload session's `doUploadData`), which is Go process
//!   state. A request that passes the gate forwards ([D-780]).
//! * **`{user_id}/image`**: every refusal is served ([`remote_set_profile_image`]); the write is
//!   `SetProfileImage`, the same pixel-exact re-encode `POST /users/{user_id}/image` forwards
//!   ([D-411]), and it forwards for the same reason.

use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use mm_model::license::License;
use mm_model::session::{SESSION_PROP_TYPE, SESSION_TYPE_REMOTECLUSTER_TOKEN, Session};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::{ServiceTokenLocation, parse_service_token, remote_id_header};
use crate::channels::require_id;
use crate::error::ApiError;
use crate::images::{
    BodyRefusal, profile_image_error, read_multipart_body, request_body_too_large,
    storage_not_configured,
};
use crate::proxy;

/// The one behaviour the four service-backed routes share: the `RemoteClusterTokenRequired` gate
/// and the session behind it, then a forward.
///
/// Registered for `ping`, `msg`, `confirm_invite` and `upload/{upload_id}`. Every refusal up to
/// and including the gate is answered here; a request that passes it reaches a handler that drives
/// the `RemoteClusterService`, which is Go's ([D-780]).
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn remote_cluster_token_gate(
    State(state): State<AppState>,
    _csrf: crate::auth::CsrfGuard,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if let Err(err) = remote_cluster_token_required(&state, &parts).await {
        return err.into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), Request::from_parts(parts, body)).await
}

/// `ServeHTTP`'s session resolution for a request that reaches a `RemoteClusterTokenRequired`
/// handler, and then the gate itself (`(*Context).RemoteClusterTokenRequired`,
/// web/context.go:159). `Ok(())` is a request past the gate.
///
/// The cookie / `Authorization` / query-string branch is [`crate::auth::CsrfGuard`], which every
/// caller extracts first: its refusals (a non-OAuth query token, a failed CSRF check) precede
/// this, as Go's `c.Err` does. Whatever session that branch finds is never a
/// `RemoteClusterToken` one, so here it is simply "not this kind".
async fn remote_cluster_token_required(state: &AppState, parts: &Parts) -> Result<(), ApiError> {
    let license = state.app.license().await.map_err(ApiError::from)?;
    let session = remote_cluster_session(state, parts, license.as_deref()).await?;

    let has_service = license.is_some_and(|license| license.has_remote_cluster_service());
    let is_remote_token = session
        .as_ref()
        .and_then(|session| session.prop(SESSION_PROP_TYPE))
        == Some(SESSION_TYPE_REMOTECLUSTER_TOKEN);
    if !has_service || !is_remote_token {
        // `license == nil || !HasRemoteClusterService() || Props[type] != RemoteClusterToken`.
        // Go's detail is the wiped `"TokenRequired"`; the wire body is the plain 401.
        return Err(ApiError::unauthenticated());
    }
    Ok(())
}

/// The cloud and remote-cluster branches of `ServeHTTP` (web/handlers.go:299-325): the session
/// they mint, `None` when neither applies, or the error Go sets as `c.Err` — which skips the gate,
/// so it is the answer.
async fn remote_cluster_session(
    state: &AppState,
    parts: &Parts,
    license: Option<&License>,
) -> Result<Option<Session>, ApiError> {
    let Some((token, location)) = parse_service_token(parts) else {
        return Ok(None);
    };
    match location {
        // `c.App.Channels().License().IsCloud()` — nil-safe in Go, and false on every licence
        // this project can sign.
        ServiceTokenLocation::CloudHeader => {
            if !license.is_some_and(License::is_cloud) {
                return Ok(None);
            }
            state
                .app
                .get_cloud_session(&token)
                .map(Some)
                .map_err(|err| ApiError::from(*err))
        }
        ServiceTokenLocation::RemoteClusterHeader => {
            if !license.is_some_and(License::has_remote_cluster_service) {
                return Ok(None);
            }
            let remote_id = remote_id_header(parts);
            if remote_id.is_empty() {
                return Err(ApiError::from(AppError::new(
                    "ServeHTTP",
                    "api.context.remote_id_missing.app_error",
                    None,
                    String::new(),
                    401,
                )));
            }
            state
                .app
                .get_remote_cluster_session(&token, &remote_id)
                .await
                .map(Some)
                .map_err(|err| ApiError::from(*err))
        }
    }
}

/// Port of `remoteSetProfileImage` (api4/remote_cluster.go:247), reached as
/// `POST /api/v4/remotecluster/{user_id}/image` by a remote server updating the picture of a user
/// it owns.
///
/// Past the gate, in Go's order:
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `user_id` is not an id | 400 `api.context.invalid_url_param.app_error` |
/// | 2 | `FileSettings.DriverName == ""` | 501 `api.user.upload_profile_user.storage.app_error` |
/// | 3 | `Content-Length` over `MaxFileSize` | 413 `api.user.upload_profile_user.too_large.app_error` |
/// | 4 | the body overran `MaxFileSize + 512` | 413 `api.context.request_body_too_large.app_error` (the parse error is wrapped) |
/// | 5 | the multipart body does not parse | **500** `api.user.upload_profile_user.parse.app_error` |
/// | 6 | no `image` file part | 400 `api.user.upload_profile_user.no_file.app_error` |
/// | 7 | the user does not exist **or is not remote** | 400 `api.context.invalid_url_param.app_error` naming `user_id` |
/// | 8 | the user's `RemoteId` is not `X-RemoteCluster-Id` | **401** `api.context.remote_id_mismatch.app_error` |
///
/// Unlike `setProfileImage` there is no permission check (the remote is the principal), no LDAP
/// picture-attribute 409 and no profile-field lock 409. Go's `len(imageArray) == 0` branch is
/// unreachable, as there: a key exists only because a part was appended under it.
///
/// Then `SetProfileImage`, which decodes, rotates, `FillCenter`s and re-encodes the upload —
/// forwarded before anything is written, the same hand-over as the local route ([D-411]); Go
/// re-runs the gate and every check above and answers from the same row.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded = false))]
pub async fn remote_set_profile_image(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    _csrf: crate::auth::CsrfGuard,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match refuse_remote_profile_image(&state, &user_id, &parts, body).await {
        Ok(bytes) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

/// Every answer `remoteSetProfileImage` gives short of the write, as `Err`; `Ok` carries the body
/// that was read, for the hand-over.
async fn refuse_remote_profile_image(
    state: &AppState,
    user_id: &str,
    parts: &Parts,
    body: axum::body::Body,
) -> Result<axum::body::Bytes, ApiError> {
    const WHERE: &str = "remoteUploadProfileImage";

    remote_cluster_token_required(state, parts).await?;

    // `c.RequireUserId()`.
    require_id(user_id, "user_id")?;

    if let Some(err) = storage_not_configured(state, WHERE) {
        return Err(err);
    }

    let (bytes, form) = match read_multipart_body(state, parts, body).await {
        Ok(parsed) => parsed,
        Err(BodyRefusal::DeclaredTooLarge) => {
            return Err(profile_image_error(
                WHERE,
                "api.user.upload_profile_user.too_large.app_error",
                413,
            ));
        }
        // `.Wrap(err)` on the parse error is what lets `handleContextError` find the
        // `MaxBytesError` and answer the global 413 instead.
        Err(BodyRefusal::ReadCapExceeded) => return Err(request_body_too_large(WHERE)),
        Err(BodyRefusal::Unparseable) => {
            return Err(profile_image_error(
                WHERE,
                "api.user.upload_profile_user.parse.app_error",
                500,
            ));
        }
    };

    if form.first_file("image").is_none() {
        return Err(profile_image_error(
            WHERE,
            "api.user.upload_profile_user.no_file.app_error",
            400,
        ));
    }

    // `if err != nil || !user.IsRemote()` — one answer for both, and `GetUser`'s own error
    // (404, or 500) is discarded.
    let user = match state.app.get_user(user_id).await {
        Ok(user) if user.is_remote() => user,
        _ => return Err(ApiError::invalid_url_param("user_id")),
    };

    // "ensure the user being modified belongs to the remote requesting the change."
    if user.get_remote_id() != remote_id_header(parts) {
        return Err(ApiError::from(AppError::new(
            "remoteSetProfileImage",
            "api.context.remote_id_mismatch.app_error",
            None,
            String::new(),
            401,
        )));
    }

    Ok(bytes)
}
