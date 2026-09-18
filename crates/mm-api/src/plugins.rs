//! The plugin routes of `api4/plugin.go` this server answers from its own plugin host.
//!
//! ```text
//! GET /api/v4/plugins            getPlugins (plugin.go:176)
//! GET /api/v4/plugins/statuses   getPluginStatuses (plugin.go:198)
//! GET /api/v4/plugins/webapp     getWebappPlugins (plugin.go:250), with no session required
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

/// Port of `getPlugins` (plugin.go:176): the same gates as the statuses, then every available
/// plugin split into `active` and `inactive`, through `json.NewEncoder`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_plugins(State(state): State<AppState>, session: AuthenticatedSession) -> Response {
    match serve_plugins(&state, &session.0).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_plugins(state: &AppState, session: &Session) -> Result<Response, ApiError> {
    if !state.app.config().plugin_enable {
        return Err(ApiError::from(AppError::new(
            "getPlugins",
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
    let response = state.app.get_plugins()?;
    crate::commands::encoded(StatusCode::OK, &response, "getPlugins")
}

/// Port of `getWebappPlugins` (plugin.go:250), an `APIHandler`: no session and no permission,
/// only the 501 when plugins are off. Each running plugin with a client part, as its client
/// manifest without the settings schema, through `json.Marshal` — so no trailing newline.
#[tracing::instrument(skip_all)]
pub async fn get_webapp_plugins(State(state): State<AppState>) -> Response {
    match serve_webapp_plugins(&state) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

fn serve_webapp_plugins(state: &AppState) -> Result<Response, ApiError> {
    if !state.app.config().plugin_enable {
        return Err(ApiError::from(AppError::new(
            "getWebappPlugins",
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        )));
    }
    let manifests = state.app.get_active_plugin_manifests()?;
    let client: Vec<mm_model::manifest::Manifest> = manifests
        .iter()
        .filter(|m| m.has_client())
        .map(|m| {
            let mut manifest = m.client_manifest();
            manifest.settings_schema = None;
            manifest
        })
        .collect();
    let body = mm_model::utils::go_json_marshal(&client).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the webapp plugins");
        ApiError::from(AppError::new(
            "getWebappPlugins",
            "api.marshal_error",
            None,
            "",
            500,
        ))
    })?;
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
