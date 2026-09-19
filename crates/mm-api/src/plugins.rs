//! The plugin routes of `api4/plugin.go` this server answers from its own plugin host.
//!
//! ```text
//! POST   /api/v4/plugins                      uploadPlugin (plugin.go:45), installPlugin (:412)
//! POST   /api/v4/plugins/install_from_url     installPluginFromURL (plugin.go:100)
//! GET    /api/v4/plugins/marketplace          getMarketplacePlugins (plugin.go:284)
//! POST   /api/v4/plugins/marketplace          installMarketplacePlugin (plugin.go:130)
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

/// The 501 every plugin route opens with while `PluginSettings.Enable` is off, under the
/// handler's own `where`.
pub(crate) fn plugins_on(state: &AppState, where_: &str) -> Result<(), ApiError> {
    if state.app.config().plugin_enable {
        return Ok(());
    }
    Err(ApiError::from(AppError::new(
        where_,
        "app.plugin.disabled.app_error",
        None,
        "",
        501,
    )))
}

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
    plugins_on(state, "getPlugins")?;
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

/// `uploadPlugin`'s one 501, for plugins off, uploads off or a signature requirement.
pub(crate) fn upload_gate(state: &AppState) -> Result<(), ApiError> {
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
    Ok(())
}

async fn serve_upload(
    state: &AppState,
    session: &Session,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    upload_gate(state)?;
    let config = state.app.config();
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

    install_plugin(state, &file.data, force).await
}

/// Port of `installPlugin` (plugin.go:412), shared by the upload and the install from a URL: the
/// plugin-directory/import-directory conflict, then the install, which answers 201 with the
/// manifest through `json.NewEncoder`.
async fn install_plugin(
    state: &AppState,
    bundle: &[u8],
    force: bool,
) -> Result<Response, ApiError> {
    let config = state.app.config();
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

    let manifest = state.app.install_plugin(bundle, force).await?;
    if let Some(manifest) = &manifest {
        tracing::Span::current().record("plugin_id", manifest.id.as_str());
    }
    crate::commands::encoded(StatusCode::CREATED, &manifest, "installPlugin")
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
    plugins_on(state, where_)?;
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
pub(crate) fn status_ok() -> Response {
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

/// `r.URL.Query()`: Go's parse, errors dropped.
fn query_values(query: Option<&str>) -> mm_model::go_url::Values {
    mm_model::go_url::parse_query(query.unwrap_or_default()).0
}

/// `Values.Get` as a string.
fn query_get(values: &mm_model::go_url::Values, key: &str) -> String {
    String::from_utf8_lossy(values.get(key).unwrap_or_default()).into_owned()
}

/// `strconv.ParseBool` with its error discarded.
fn query_bool(values: &mm_model::go_url::Values, key: &str) -> bool {
    crate::user_deletes::parse_go_bool(&query_get(values, key))
}

/// Why `parseMarketplacePluginFilter` refused the query. Go wraps it into the 500
/// `app.plugin.marshal.app_error`, so only the log sees the text.
#[derive(Debug, thiserror::Error)]
enum FilterError {
    #[error("failed to parse {0} as integer")]
    NotAnInteger(&'static str),
    #[error("local_only and remote_only cannot be both true")]
    LocalAndRemote,
}

/// `parseInt` (api4/helpers.go:32): absent or empty is the default, anything else `strconv.Atoi`.
fn parse_int(
    values: &mm_model::go_url::Values,
    name: &'static str,
    default: i64,
) -> Result<i64, FilterError> {
    let raw = query_get(values, name);
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<i64>()
        .map_err(|_| FilterError::NotAnInteger(name))
}

/// `parseMarketplacePluginFilter` as `getMarketplacePlugins` answers its failure: the 500
/// `app.plugin.marshal.app_error`, ahead of the permission check.
pub(crate) fn marketplace_filter(
    query: Option<&str>,
) -> Result<mm_model::marketplace_plugin::MarketplacePluginFilter, ApiError> {
    parse_marketplace_plugin_filter(query).map_err(|err| {
        ApiError::from(
            AppError::new(
                GET_MARKETPLACE,
                "app.plugin.marshal.app_error",
                None,
                "",
                500,
            )
            .wrap(err),
        )
    })
}

/// Port of `parseMarketplacePluginFilter` (plugin.go:382).
fn parse_marketplace_plugin_filter(
    query: Option<&str>,
) -> Result<mm_model::marketplace_plugin::MarketplacePluginFilter, FilterError> {
    let values = query_values(query);
    let page = parse_int(&values, "page", 0)?;
    let per_page = parse_int(&values, "per_page", 100)?;
    let local_only = query_bool(&values, "local_only");
    let remote_only = query_bool(&values, "remote_only");
    if local_only && remote_only {
        return Err(FilterError::LocalAndRemote);
    }
    Ok(mm_model::marketplace_plugin::MarketplacePluginFilter {
        page,
        per_page,
        filter: query_get(&values, "filter"),
        server_version: query_get(&values, "server_version"),
        local_only,
        remote_only,
        ..Default::default()
    })
}

pub(crate) const GET_MARKETPLACE: &str = "getMarketplacePlugins";

/// The two 501s the marketplace routes open with, each under its handler's `where`.
pub(crate) fn marketplace_gates(state: &AppState, where_: &str) -> Result<(), ApiError> {
    let config = state.app.config();
    if !config.plugin_enable {
        return Err(ApiError::from(AppError::new(
            where_,
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        )));
    }
    if !config.plugin_enable_marketplace {
        return Err(ApiError::from(AppError::new(
            where_,
            "app.plugin.marketplace_disabled.app_error",
            None,
            "",
            501,
        )));
    }
    Ok(())
}

/// Port of `getMarketplacePlugins` (plugin.go:284).
///
/// Go's order: plugins off, then the Marketplace off (both 501); the filter, whose failure — a
/// non-integer `page`/`per_page`, or `local_only` with `remote_only` — is the 500
/// `app.plugin.marshal.app_error` and comes **before** the permission; then
/// `sysconsole_read_plugins`, which a `remote_only` request skips; then the merged list through
/// `json.Marshal`, so no trailing newline, and `null` when nothing matches. `page`, `per_page`
/// and `server_version` are parsed and then unused, as in Go. See `mm_app::marketplace`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_marketplace_plugins(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    match serve_marketplace(&state, &session.0, query.as_deref()).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_marketplace(
    state: &AppState,
    session: &Session,
    query: Option<&str>,
) -> Result<Response, ApiError> {
    marketplace_gates(state, GET_MARKETPLACE)?;
    let filter = marketplace_filter(query)?;
    if !filter.remote_only
        && !state
            .app
            .session_has_permission_to(session, &PERMISSION_SYSCONSOLE_READ_PLUGINS)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&PERMISSION_SYSCONSOLE_READ_PLUGINS],
        )));
    }
    let plugins = state.app.get_marketplace_plugins(&filter).await?;
    let body = if plugins.is_empty() {
        // `json.Marshal` of a nil slice.
        "null".to_owned()
    } else {
        mm_model::utils::go_json_marshal(&plugins).map_err(|err| {
            ApiError::from(
                AppError::new(
                    GET_MARKETPLACE,
                    "app.plugin.marshal.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(err),
            )
        })?
    };
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

pub(crate) const INSTALL_MARKETPLACE: &str = "installMarketplacePlugin";

/// The `json:` names of `InstallMarketplacePluginRequest`, for Go's case-insensitive match.
const INSTALL_REQUEST_FIELDS: mm_model::go_json::GoFields = mm_model::go_json::GoFields {
    names: &["id", "version"],
    nested: &[],
};

/// `PluginRequestFromReader` (marketplace_plugin.go:127): `json.NewDecoder(…).Decode(&r)` into a
/// pointer — the first JSON value, keys matched case-insensitively. `None` is a failure.
///
/// A body of `null` decodes into a nil pointer, which Go then dereferences: measured, the
/// oracle's connection closes with no response at all. That cannot be reproduced here; it is the
/// same 501 as a body that does not decode.
fn plugin_request_from_json(
    bytes: &[u8],
) -> Option<mm_model::marketplace_plugin::InstallMarketplacePluginRequest> {
    let mut value: serde_json::Value = mm_model::utils::decode_one_from_json(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    mm_model::go_json::remap_object_keys(&mut value, &INSTALL_REQUEST_FIELDS);
    serde_json::from_value(value).ok()
}

/// `PluginRequestFromReader` as `installMarketplacePlugin` answers its failure: a **501**.
pub(crate) fn marketplace_request(
    body: &[u8],
) -> Result<mm_model::marketplace_plugin::InstallMarketplacePluginRequest, ApiError> {
    plugin_request_from_json(body).ok_or_else(marketplace_request_refused)
}

/// The 501 `installMarketplacePlugin` gives a body that does not decode.
pub(crate) fn marketplace_request_refused() -> ApiError {
    ApiError::from(AppError::new(
        INSTALL_MARKETPLACE,
        "app.plugin.marketplace_plugin_request.app_error",
        None,
        "",
        501,
    ))
}

/// Port of `installMarketplacePlugin` (plugin.go:130).
///
/// Go's order: plugins off, the Marketplace off (the two 501s); `sysconsole_write_plugins`; the
/// body, whose failure is — unusually — a **501**, `app.plugin.marketplace_plugin_request.app_error`;
/// the requested version is then discarded, so the latest compatible one is always installed
/// (MM-41981); then the install (`mm_app::marketplace`), and 201 with the manifest through
/// `json.NewEncoder`. Everything here is public code; nothing is forwarded.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn install_marketplace_plugin(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    body: axum::body::Bytes,
) -> Response {
    let result = async {
        marketplace_gates(&state, INSTALL_MARKETPLACE)?;
        if !state
            .app
            .session_has_permission_to(
                &session.0,
                &mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS,
            )
            .await
        {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS],
            )));
        }
        let mut request = marketplace_request(&body)?;
        request.version.clear();
        let manifest = state.app.install_marketplace_plugin(&request).await?;
        crate::commands::encoded(StatusCode::CREATED, &manifest, INSTALL_MARKETPLACE)
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// `installPluginFromURL`'s one 501 — `app.plugin.disabled.app_error`, not the upload one — for
/// plugins off, a signature requirement or uploads off.
pub(crate) fn from_url_gate(state: &AppState) -> Result<(), ApiError> {
    let config = state.app.config();
    if !config.plugin_enable || config.plugin_require_signature || !config.plugin_enable_uploads {
        return Err(ApiError::from(AppError::new(
            "installPluginFromURL",
            "app.plugin.disabled.app_error",
            None,
            "",
            501,
        )));
    }
    Ok(())
}

/// Port of `installPluginFromURL` (plugin.go:100).
///
/// One 501, `app.plugin.disabled.app_error`, for plugins off, a signature requirement or uploads
/// off; `sysconsole_write_plugins`; `force` through `strconv.ParseBool` with its error dropped;
/// `plugin_download_url` fetched by `downloadFromURL` — any failure, including an `http` URL
/// while `AllowInsecureDownloadURL` is off, is the 400
/// `api.plugin.install.download_failed.app_error`; then [`install_plugin`], as the upload.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, force, plugin_id))]
pub async fn install_plugin_from_url(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let result = async {
        from_url_gate(&state)?;
        if !state
            .app
            .session_has_permission_to(
                &session.0,
                &mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS,
            )
            .await
        {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&mm_model::permission::PERMISSION_SYSCONSOLE_WRITE_PLUGINS],
            )));
        }
        let values = query_values(query.as_deref());
        let force = query_bool(&values, "force");
        tracing::Span::current().record("force", force);
        let download_url = query_get(&values, "plugin_download_url");
        let bundle = state
            .app
            .download_from_url(&download_url)
            .await
            .map_err(|err| {
                ApiError::from(
                    AppError::new(
                        "installPluginFromURL",
                        "api.plugin.install.download_failed.app_error",
                        None,
                        "",
                        400,
                    )
                    .wrap(err),
                )
            })?;
        install_plugin(&state, &bundle, force).await
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_parses_as_go_does() {
        let f = parse_marketplace_plugin_filter(None).unwrap();
        assert_eq!((f.page, f.per_page), (0, 100));
        let f = parse_marketplace_plugin_filter(Some(
            "page=2&per_page=&filter=a+b&server_version=9&local_only=T&remote_only=yes",
        ))
        .unwrap();
        assert_eq!((f.page, f.per_page), (2, 100));
        assert_eq!(f.filter, "a b");
        assert_eq!(f.server_version, "9");
        assert!(f.local_only && !f.remote_only);
        assert!(parse_marketplace_plugin_filter(Some("page=x")).is_err());
        assert!(parse_marketplace_plugin_filter(Some("per_page=1.5")).is_err());
        assert!(parse_marketplace_plugin_filter(Some("local_only=1&remote_only=true")).is_err());
    }

    #[test]
    fn the_install_request_decodes_as_go_does() {
        let r = plugin_request_from_json(br#"{"ID":"x","Version":"1"} trailing"#).unwrap();
        assert_eq!((r.id.as_str(), r.version.as_str()), ("x", "1"));
        for bad in [&b""[..], b"null", b"[]", br#"{"id":5}"#, b"{"] {
            assert!(plugin_request_from_json(bad).is_none(), "{bad:?}");
        }
    }
}
