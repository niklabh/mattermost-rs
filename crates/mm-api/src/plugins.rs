//! The plugin routes of `api4/plugin.go` this server answers from its own plugin host.
//!
//! ```text
//! GET /api/v4/plugins/statuses   getPluginStatuses (plugin.go:198)
//! ```
//!
//! # Registered only when this process hosts plugins
//!
//! The statuses are the running environment's, so with `MMRS_PLUGIN_HOST` unset or `go` the
//! plugins run in Go and only Go can answer: the route is not registered, and the request falls
//! to the proxy before any extractor sees it, so even an anonymous caller gets Go's own 401. With
//! `rust`, see [`mm_app::plugins`].

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_SYSCONSOLE_READ_PLUGINS, make_permission_error};
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `getPluginStatuses` (plugin.go:198): the 501 when plugins are off comes before the
/// permission check, then `sysconsole_read_plugins`, then the statuses through
/// `json.NewEncoder`, so with a trailing newline.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_plugin_statuses(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    match serve_statuses(&state, &session.0).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_statuses(state: &AppState, session: &Session) -> Result<Response, ApiError> {
    if !state.app.config().plugin_enable {
        return Err(ApiError::from(AppError::new(
            "getPluginStatuses",
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        )));
    }
    if !state
        .app
        .session_has_permission_to(session, &PERMISSION_SYSCONSOLE_READ_PLUGINS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&PERMISSION_SYSCONSOLE_READ_PLUGINS],
        )));
    }
    let statuses = state.app.get_cluster_plugin_statuses()?;
    crate::commands::encoded(StatusCode::OK, &statuses, "getPluginStatuses")
}
