//! The two `system_local.go` pairs of the system-operations family on the unix socket:
//! `POST /api/v4/integrity` (`localCheckIntegrity`, a local-only handler) and
//! `GET /api/v4/logs` (`getLogs` through `APILocal`, the HTTP handler under the local session,
//! whose `SessionHasPermissionToAndNotRestrictedAdmin` gate a local session passes outright).
//! See [`crate::local`] for the transport and [`crate::sysops`] for the handlers.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::{get, post};

use crate::error::ApiError;
use crate::local::{local_session, partially_migrated};
use crate::{AppState, sysops};

/// The two registrations, merged into [`crate::local::router`].
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // `api.BaseRoutes.APIRoot.Handle("/integrity", api.APILocal(localCheckIntegrity))`
        // (system_local.go:21).
        .route(
            "/api/v4/integrity",
            partially_migrated(post(sysops::local_check_integrity)),
        )
        // `api.BaseRoutes.APIRoot.Handle("/logs", api.APILocal(getLogs))` (system_local.go:16) —
        // the `GET` alone; the local router registers no `POST /logs`.
        .route("/api/v4/logs", partially_migrated(get(local_get_logs)))
}

/// `getLogs` through `APILocal`.
async fn local_get_logs(state: State<AppState>, query: RawQuery) -> Result<Response, ApiError> {
    sysops::get_logs(state, query, local_session()).await
}
