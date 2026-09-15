//! Port of the two `first_admin_visit` routes in `api4/plugin.go`:
//!
//! ```text
//! POST /api/v4/plugins/marketplace/first_admin_visit   setFirstAdminVisitMarketplaceStatus (:434)
//! GET  /api/v4/plugins/marketplace/first_admin_visit   getFirstAdminVisitMarketplaceStatus (:462)
//! ```
//!
//! # These two are the only `/plugins` routes this server answers, and the path is a literal
//!
//! Everything else under `BaseRoutes.Plugins` needs the plugin environment and is Go's. Registering
//! one literal path beside nothing else served is safe because axum, like gorilla/mux, matches the
//! full literal before `{plugin_id}` could — and `first_admin_visit` sits under `/marketplace/`,
//! a segment no plugin id can spell. The parity suite proves the neighbours still forward:
//! `GET /plugins`, `GET /plugins/marketplace`, `GET /plugins/statuses`, `GET /plugins/webapp`,
//! `POST /plugins/marketplace` and `POST /plugins/install_from_url` all come back Go's.
//!
//! # The `POST` is `APIHandler`, the `GET` is `APISessionRequired`
//!
//! So an anonymous `POST` is not a 401: the handler runs with the zero session, the
//! `manage_system` check fails, and the answer is the ordinary 403 with `userId=` empty in the
//! detail. The `GET` beside it refuses the same caller at the wrapper with a 401. Both are
//! measured against Go.
//!
//! # Neither reads the plugin host
//!
//! The row is a `Systems` entry; the write publishes one websocket event. See
//! [`mm_app::App::first_admin_visit_marketplace_status`] and its setter for the store-error
//! mapping, which is the only logic here besides the permission gate.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error_for_user};
use mm_model::session::Session;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::auth_writes::OptionalSession;
use crate::error::ApiError;

/// `SessionHasPermissionTo(session, manage_system)` on whichever session the wrapper produced,
/// with the zero session standing in for "none" exactly as Go's `c.AppContext.Session()` does.
async fn require_manage_system(state: &AppState, session: &Session) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error_for_user(
        &session.user_id,
        &[&PERMISSION_MANAGE_SYSTEM],
    )))
}

/// Port of `getFirstAdminVisitMarketplaceStatus` (plugin.go:462) — the JSON of the row, or of
/// the synthesised `"false"`, through `json.NewEncoder` and so with a trailing newline.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_first_admin_visit_marketplace_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    match serve_get(&state, &session.0).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_get(state: &AppState, session: &Session) -> Result<Response, ApiError> {
    require_manage_system(state, session).await?;
    let row = state.app.first_admin_visit_marketplace_status().await?;
    crate::commands::encoded(
        axum::http::StatusCode::OK,
        &row,
        "getFirstAdminVisitMarketplaceStatus",
    )
}

/// Port of `setFirstAdminVisitMarketplaceStatus` (plugin.go:434) — `ReturnStatusOK` after the
/// upsert and the broadcast. The body of the request is never read.
#[tracing::instrument(skip_all, fields(user_id))]
pub async fn set_first_admin_visit_marketplace_status(
    State(state): State<AppState>,
    session: OptionalSession,
) -> Response {
    let session = session.0.unwrap_or_default();
    tracing::Span::current().record("user_id", &session.user_id);
    match serve_set(&state, &session).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_set(state: &AppState, session: &Session) -> Result<Response, ApiError> {
    require_manage_system(state, session).await?;
    state.app.set_first_admin_visit_marketplace_status().await?;
    Ok(crate::channel_writes::status_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anonymous `POST` reaches the permission check with the zero session, and the detail Go
    /// logs names an empty user — not a 401, which is what a session-required wrapper would give.
    #[test]
    fn an_absent_session_is_a_403_naming_nobody() {
        let session = OptionalSession(None).0.unwrap_or_default();
        let err = make_permission_error_for_user(&session.user_id, &[&PERMISSION_MANAGE_SYSTEM]);
        assert_eq!(err.status_code, 403);
        assert_eq!(err.id, "api.context.permissions.app_error");
        assert_eq!(err.detailed_error, "userId=, permission=manage_system");
    }
}
