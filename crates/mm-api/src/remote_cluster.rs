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
//! # On this build every one of them is a 401, before any handler code runs
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
//! # What is forwarded, and why it cannot be served here
//!
//! When a licence **with** the remote-cluster service is present the gate's first two disjuncts
//! pass and the answer turns on the session: the web layer resolves an `X-RemoteCluster-Token`
//! against the `RemoteClusters` table (`GetRemoteClusterSession`, session.go:71) and a request
//! with no matching row is the *different* 401 `api.context.invalid_token.error` — measured on
//! the licensed oracle. Minting that session needs a planted `RemoteClusters` row and the remote
//! cluster session path, neither of which is ported, so a licensed request is **forwarded**: Go
//! resolves the token and answers. On this build that branch is unreachable — there is no licence
//! — so nothing is forwarded in practice, but the decision is the licence, read live. See
//! [D-780].

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::ApiError;
use crate::proxy;

/// The one behaviour all five routes share on this build: the `RemoteClusterTokenRequired` gate.
///
/// Registered for every method+path pair in the family; the handlers behind the gate are never
/// reached without a licence, so one function is the whole served surface.
#[tracing::instrument(skip_all, fields(licensed_service, forwarded = false))]
pub async fn remote_cluster_token_gate(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    // `c.App.Channels().License()` and `.HasRemoteClusterService()`.
    let has_service = match state.app.license().await {
        Ok(license) => license.is_some_and(|license| license.has_remote_cluster_service()),
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("licensed_service", has_service);

    if !has_service {
        // `license == nil || !HasRemoteClusterService()` — the first disjunct on this build.
        // Go's detail is the wiped `"TokenRequired"`; the wire body is the plain 401.
        return ApiError::unauthenticated().into_response();
    }

    // A licence with the service is present: the answer now depends on resolving the remote
    // cluster session, which is Go's. Unreachable on this build.
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}
