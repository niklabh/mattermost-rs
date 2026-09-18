//! The plugin routes of `api4/plugin.go` this server answers from its own plugin host.
//!
//! ```text
//! POST   /api/v4/plugins                      uploadPlugin (plugin.go:45), installPlugin (:412)
//! DELETE /api/v4/plugins/{plugin_id}          removePlugin (plugin.go:220)
//! POST   /api/v4/plugins/{plugin_id}/enable   enablePlugin (plugin.go:324)
//! POST   /api/v4/plugins/{plugin_id}/disable  disablePlugin (plugin.go:353)
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

/// Port of `getWebappPlugins` (plugin.go:250) while the plugins run in **Go** — the default,
/// `MMRS_PLUGIN_HOST` unset or `go` — when [`get_webapp_plugins`] is not registered.
///
/// The list is Go's running plugins, which only Go knows. Two answers do not need it: plugins off
/// is the 501 whatever runs, and a Go plugin directory holding no bundle proves there is no
/// running plugin, so the list is `[]` (`json.Marshal` of an empty slice, no newline) — the proof
/// `commands::go_may_have_plugins` already gives the slash-command routes. Anything else, or not
/// knowing the directory, forwards: that list depends on state only the Go process holds.
#[tracing::instrument(skip_all)]
pub async fn get_webapp_plugins_go_hosted(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Response {
    if !state.app.config().plugin_enable {
        return ApiError::from(AppError::new(
            "getWebappPlugins",
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        ))
        .into_response();
    }
    if crate::commands::go_may_have_plugins() {
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        "[]",
    )
        .into_response()
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

/// Port of `uploadPlugin` (plugin.go:45) and `installPlugin` (plugin.go:412).
///
/// The gates in Go's order: plugins, uploads and no signature requirement (one 501 for all
/// three); `sysconsole_write_plugins`; the body. The body is capped at `MaxFileSize + 512`
/// (`FileAPI`, web/handlers.go:217): over it is the 413
/// `api.plugin.upload.file_too_large.app_error`; any other parse failure is written with
/// `http.Error`, as plain text carrying Go's error, not as an `AppError`. Then the `plugin` file
/// part, `force` (the literal `"true"`), the plugin-directory/import-directory conflict, and the
/// install, which answers 201 with the manifest.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, force, plugin_id))]
pub async fn upload_plugin(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    match serve_upload(&state, &session.0, request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

const UPLOAD: &str = "uploadPlugin";

async fn serve_upload(
    state: &AppState,
    session: &Session,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let config = state.app.config();
    if !config.plugin_enable || !config.plugin_enable_uploads || config.plugin_require_signature {
        return Err(ApiError::from(AppError::new(
            UPLOAD,
            "app.plugin.upload_disabled.app_error",
            None,
            "",
            501,
        )));
    }
    if !state
        .app
        .session_has_permission_to(
            session,
            &mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS],
        )));
    }

    let (parts, body) = request.into_parts();
    let cap = config
        .file_max_file_size
        .saturating_add(crate::images::BYTES_MIN_READ);
    let limit = usize::try_from(cap).unwrap_or(usize::MAX);
    let bytes = match axum::body::to_bytes(body, limit.saturating_add(1)).await {
        Ok(bytes) if bytes.len() <= limit => bytes,
        // `http: request body too large` from the `MaxBytesReader`.
        _ => {
            return Err(ApiError::from(AppError::new(
                UPLOAD,
                "api.plugin.upload.file_too_large.app_error",
                None,
                "",
                413,
            )));
        }
    };
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let form = match crate::multipart::parse_form(content_type, &bytes) {
        Ok(form) => form,
        Err(err) => return Ok(http_error(err.go_text(), StatusCode::BAD_REQUEST)),
    };

    let Some(files) = form.file.get("plugin") else {
        return Err(ApiError::from(AppError::new(
            UPLOAD,
            "api.plugin.upload.no_file.app_error",
            None,
            "",
            400,
        )));
    };
    let Some(file) = files.first() else {
        return Err(ApiError::from(AppError::new(
            UPLOAD,
            "api.plugin.upload.array.app_error",
            None,
            "",
            400,
        )));
    };
    let force = form
        .value
        .get("force")
        .and_then(|v| v.first())
        .is_some_and(|v| v == "true");
    tracing::Span::current().record("force", force);

    match mm_app::App::check_directory_conflict(&config.plugin_directory, &config.import_directory)
    {
        Err(err) => {
            return Err(ApiError::from(
                AppError::new(
                    "installPlugin",
                    "api.plugin.install.check_directory.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(err),
            ));
        }
        Ok(true) => {
            return Err(ApiError::from(AppError::new(
                "installPlugin",
                "api.plugin.install.directory_conflict.app_error",
                None,
                "",
                403,
            )));
        }
        Ok(false) => {}
    }

    let manifest = state.app.install_plugin(&file.data, force).await?;
    if let Some(manifest) = &manifest {
        tracing::Span::current().record("plugin_id", manifest.id.as_str());
    }
    crate::commands::encoded(StatusCode::CREATED, &manifest, UPLOAD)
}

/// Go's `http.Error`: a plain-text body with a newline, `nosniff`, and no `AppError`.
fn http_error(message: &str, status: StatusCode) -> Response {
    (
        status,
        [
            ("Content-Type", "text/plain; charset=utf-8"),
            ("X-Content-Type-Options", "nosniff"),
            ("x-mmrs-served-by", "rust"),
        ],
        format!("{message}\n"),
    )
        .into_response()
}

/// The three writes on one plugin share Go's shape: `RequirePluginId`, the 501 when plugins are
/// off (each under its own `where`), `sysconsole_write_plugins`, the app call, and
/// `ReturnStatusOK`. The id is non-empty by routing.
async fn plugin_write(state: &AppState, session: &Session, where_: &str) -> Result<(), ApiError> {
    if !state.app.config().plugin_enable {
        return Err(ApiError::from(AppError::new(
            where_,
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        )));
    }
    if !state
        .app
        .session_has_permission_to(
            session,
            &mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS],
        )));
    }
    Ok(())
}

/// `ReturnStatusOK`: `{"status":"OK"}` with no trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// Port of `enablePlugin` (plugin.go:324).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, plugin_id = %plugin_id))]
pub async fn enable_plugin(
    State(state): State<AppState>,
    axum::extract::Path(plugin_id): axum::extract::Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let result = async {
        plugin_write(&state, &session.0, "activatePlugin").await?;
        state.app.enable_plugin(&plugin_id).await?;
        Ok::<_, ApiError>(status_ok())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// Port of `disablePlugin` (plugin.go:353).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, plugin_id = %plugin_id))]
pub async fn disable_plugin(
    State(state): State<AppState>,
    axum::extract::Path(plugin_id): axum::extract::Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let result = async {
        plugin_write(&state, &session.0, "deactivatePlugin").await?;
        state.app.disable_plugin(&plugin_id).await?;
        Ok::<_, ApiError>(status_ok())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// Port of `removePlugin` (plugin.go:220).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, plugin_id = %plugin_id))]
pub async fn remove_plugin(
    State(state): State<AppState>,
    axum::extract::Path(plugin_id): axum::extract::Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let result = async {
        plugin_write(&state, &session.0, "removePlugin").await?;
        state.app.remove_plugin(&plugin_id).await?;
        Ok::<_, ApiError>(status_ok())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}
