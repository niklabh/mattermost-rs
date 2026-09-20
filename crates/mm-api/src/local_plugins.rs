//! `plugin_local.go` on the unix socket: all ten registrations of `InitPluginLocal`.
//!
//! ```text
//! POST   /api/v4/plugins                      uploadPlugin
//! GET    /api/v4/plugins                      getPlugins
//! POST   /api/v4/plugins/install_from_url     installPluginFromURL
//! DELETE /api/v4/plugins/{plugin_id}          removePlugin
//! POST   /api/v4/plugins/{plugin_id}/enable   enablePlugin
//! POST   /api/v4/plugins/{plugin_id}/disable  disablePlugin
//! POST   /api/v4/plugins/marketplace          installMarketplacePlugin
//! GET    /api/v4/plugins/marketplace          getMarketplacePlugins
//! POST   /api/v4/plugins/reattach             reattachPlugin      (local only)
//! POST   /api/v4/plugins/{plugin_id}/detach   detachPlugin        (local only)
//! ```
//!
//! Eight are the HTTP handlers of [`crate::plugins`] under [`local_session`]: their permission
//! checks are all `SessionHasPermissionTo`, which the local session passes, and nothing else in
//! them reads the session. Their config gates still apply. The two local-only ones are
//! [`local_reattach_plugin`] and [`local_detach_plugin`].
//!
//! # Which process answers depends on where the plugins run
//!
//! Every one of these routes ends in the plugin environment, and there is one environment: Go's,
//! or — with `MMRS_PLUGIN_HOST=rust` — this server's ([`mm_app::plugins`]).
//!
//! - **Rust hosts the plugins:** every branch is answered here.
//! - **Go hosts them (the default):** what can be decided without the environment is answered
//!   here — each handler's config 501s, the marketplace filter's 500, the marketplace body's 501,
//!   and reattach's decoder and `IsValid` 400s — and the rest is forwarded over Go's socket,
//!   because it reads or changes plugins only the Go process runs. That forward is permanent for
//!   as long as Go is the host; it is the same line the HTTP router draws, where the whole family
//!   is forwarded under a Go host ([`crate::router`]).

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use mm_model::plugin_reattach::PluginReattachRequest;
use mm_model::utils::AppError;

use crate::error::ApiError;
use crate::local::{
    GoLocalSocket, forward_over_unix, local_session, partially_migrated,
    partially_migrated_with_ids,
};
use crate::{AppState, plugins};

/// The ten registrations, merged into [`crate::local::router`]; which handlers depends on the
/// plugin host (see the module docs).
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    let hosted = state.app.plugin_host().hosted();
    // `api.BaseRoutes.Plugins.Handle("", …)` (plugin_local.go:14-15).
    let root = if hosted {
        get(local_get_plugins).post(local_upload_plugin)
    } else {
        get(go_hosted_get_plugins).post(go_hosted_upload_plugin)
    };
    let from_url = if hosted {
        post(local_install_plugin_from_url)
    } else {
        post(go_hosted_install_plugin_from_url)
    };
    let marketplace = if hosted {
        get(local_get_marketplace_plugins).post(local_install_marketplace_plugin)
    } else {
        get(go_hosted_get_marketplace_plugins).post(go_hosted_install_marketplace_plugin)
    };
    let one = if hosted {
        delete(local_remove_plugin)
    } else {
        delete(go_hosted_remove_plugin)
    };
    let enable = if hosted {
        post(local_enable_plugin)
    } else {
        post(go_hosted_enable_plugin)
    };
    let disable = if hosted {
        post(local_disable_plugin)
    } else {
        post(go_hosted_disable_plugin)
    };
    Router::new()
        .route("/api/v4/plugins", partially_migrated(root))
        .route(
            "/api/v4/plugins/install_from_url",
            partially_migrated(from_url),
        )
        .route(
            "/api/v4/plugins/marketplace",
            partially_migrated(marketplace),
        )
        .route(
            "/api/v4/plugins/reattach",
            partially_migrated(post(local_reattach_plugin)),
        )
        .route(
            "/api/v4/plugins/{plugin_id}",
            partially_migrated_with_ids(state, one),
        )
        .route(
            "/api/v4/plugins/{plugin_id}/enable",
            partially_migrated_with_ids(state, enable),
        )
        .route(
            "/api/v4/plugins/{plugin_id}/disable",
            partially_migrated_with_ids(state, disable),
        )
        .route(
            "/api/v4/plugins/{plugin_id}/detach",
            partially_migrated_with_ids(state, post(local_detach_plugin)),
        )
}

// ---- Rust hosts the plugins: the HTTP handlers under the local session. ----

/// `getPlugins` through `APILocal` (plugin_local.go:15).
async fn local_get_plugins(state: State<AppState>) -> Response {
    plugins::get_plugins(state, local_session()).await
}

/// `uploadPlugin` through `APILocal` with `handlerParamFileAPI` (plugin_local.go:14): the same
/// `MaxFileSize` body cap as the HTTP route.
async fn local_upload_plugin(state: State<AppState>, request: Request) -> Response {
    plugins::upload_plugin(state, local_session(), request).await
}

/// `installPluginFromURL` through `APILocal` (plugin_local.go:16).
async fn local_install_plugin_from_url(state: State<AppState>, query: RawQuery) -> Response {
    plugins::install_plugin_from_url(state, local_session(), query).await
}

/// `getMarketplacePlugins` through `APILocal` (plugin_local.go:21).
async fn local_get_marketplace_plugins(state: State<AppState>, query: RawQuery) -> Response {
    plugins::get_marketplace_plugins(state, local_session(), query).await
}

/// `installMarketplacePlugin` through `APILocal` (plugin_local.go:20).
async fn local_install_marketplace_plugin(
    state: State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    plugins::install_marketplace_plugin(state, local_session(), body).await
}

/// `removePlugin` through `APILocal` (plugin_local.go:17).
async fn local_remove_plugin(state: State<AppState>, id: Path<String>) -> Response {
    plugins::remove_plugin(state, id, local_session()).await
}

/// `enablePlugin` through `APILocal` (plugin_local.go:18).
async fn local_enable_plugin(state: State<AppState>, id: Path<String>) -> Response {
    plugins::enable_plugin(state, id, local_session()).await
}

/// `disablePlugin` through `APILocal` (plugin_local.go:19).
async fn local_disable_plugin(state: State<AppState>, id: Path<String>) -> Response {
    plugins::disable_plugin(state, id, local_session()).await
}

// ---- Go hosts the plugins: the gates here, the rest over Go's socket. ----

/// Answer `gate`'s refusal, or forward the request untouched.
async fn gate_or_forward(
    gate: Result<(), ApiError>,
    go: &GoLocalSocket,
    request: Request,
) -> Response {
    match gate {
        Err(err) => err.into_response(),
        Ok(()) => forward_over_unix(&go.0, request).await,
    }
}

/// `getPlugins` under a Go host: the 501, else Go's list.
async fn go_hosted_get_plugins(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(plugins::plugins_on(&state, "getPlugins"), &go, request).await
}

/// `uploadPlugin` under a Go host: the upload 501, else Go installs it.
async fn go_hosted_upload_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(plugins::upload_gate(&state), &go, request).await
}

/// `installPluginFromURL` under a Go host: the 501, else Go downloads and installs.
async fn go_hosted_install_plugin_from_url(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(plugins::from_url_gate(&state), &go, request).await
}

/// `getMarketplacePlugins` under a Go host: the two 501s and the filter's 500, else Go's list —
/// which merges in Go's installed plugins.
async fn go_hosted_get_marketplace_plugins(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let gate = plugins::marketplace_gates(&state, plugins::GET_MARKETPLACE)
        .and_then(|()| plugins::marketplace_filter(request.uri().query()).map(drop));
    gate_or_forward(gate, &go, request).await
}

/// `installMarketplacePlugin` under a Go host: the two 501s and the body's 501 — the permission
/// between them passes locally — else Go installs.
async fn go_hosted_install_marketplace_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    if let Err(err) = plugins::marketplace_gates(&state, plugins::INSTALL_MARKETPLACE) {
        return err.into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => return unread(&err, plugins::marketplace_request_refused()),
    };
    if let Err(err) = plugins::marketplace_request(&bytes) {
        return err.into_response();
    }
    forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// `removePlugin` under a Go host.
async fn go_hosted_remove_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(plugins::plugins_on(&state, "removePlugin"), &go, request).await
}

/// `enablePlugin` under a Go host.
async fn go_hosted_enable_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(plugins::plugins_on(&state, "activatePlugin"), &go, request).await
}

/// `disablePlugin` under a Go host.
async fn go_hosted_disable_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    gate_or_forward(
        plugins::plugins_on(&state, "deactivatePlugin"),
        &go,
        request,
    )
    .await
}

/// A body that could not be read — the client went away mid-request — answered as the handler's
/// decoder failure, which is what Go's decoder reports it as.
fn unread(err: &axum::Error, answer: ApiError) -> Response {
    tracing::debug!(error = %err, "could not read a local plugin request body");
    answer.into_response()
}

/// `reattachPlugin`'s decoder failure: the 400 `api4.plugin.reattachPlugin.invalid_request`.
fn reattach_invalid_request() -> AppError {
    AppError::new(
        "reattachPlugin",
        "api4.plugin.reattachPlugin.invalid_request",
        None,
        "",
        400,
    )
}

// ---- The two local-only handlers. ----

/// Go's handlers that write nothing: a 200 with no body, `Content-Type` set by `ServeHTTP`.
fn empty_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        "",
    )
        .into_response()
}

/// Port of `reattachPlugin` (api4/plugin_local.go:29), which exists only on the socket.
///
/// Go's order: the body through `json.NewDecoder` (any failure, an empty body included, is the
/// 400 `api4.plugin.reattachPlugin.invalid_request`); `PluginReattachRequest.IsValid` (the 400s
/// for a missing manifest, then a missing config); then `App.ReattachPlugin` — the 501 with
/// plugins off, a detach of the id, and the reattach, whose one error is the 500 for a manifest
/// with no server. Success is a 200 with **no body**: the handler never writes one.
///
/// **Under a Go host the app call is forwarded.** It binds a plugin process to Go's environment,
/// which only the Go process holds. The 400s and the 501 need no environment and are answered
/// here; the forward carries the body as it arrived. `docs/TECH_DEBT.md` D-850.
#[tracing::instrument(skip_all, fields(plugin_id))]
async fn local_reattach_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => return unread(&err, ApiError::from(reattach_invalid_request())),
    };
    let reattach = match PluginReattachRequest::from_json(&bytes) {
        Ok(reattach) => reattach,
        Err(err) => return ApiError::from(reattach_invalid_request().wrap(err)).into_response(),
    };
    if let Err(err) = reattach.is_valid() {
        return ApiError::from(err).into_response();
    }
    let (Some(manifest), Some(config)) = (reattach.manifest, reattach.plugin_reattach_config)
    else {
        // `is_valid` has just refused both absences.
        return ApiError::from(reattach_invalid_request()).into_response();
    };
    tracing::Span::current().record("plugin_id", manifest.id.as_str());

    if !state.app.plugin_host().hosted() {
        if let Err(err) = plugins::plugins_on(&state, "ReattachPlugin") {
            return err.into_response();
        }
        return forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await;
    }
    match state.app.reattach_plugin(&manifest, &config).await {
        Ok(()) => empty_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `detachPlugin` (api4/plugin_local.go:52), which exists only on the socket:
/// `RequirePluginId` (non-empty by routing), then `App.DetachPlugin` — the 501 with plugins off,
/// otherwise deactivate and forget, which succeeds for any id. A 200 with no body.
///
/// **Under a Go host the detach is forwarded**, after the 501: the plugin it stops runs in the Go
/// process. `docs/TECH_DEBT.md` D-850.
#[tracing::instrument(skip_all, fields(plugin_id = %plugin_id))]
async fn local_detach_plugin(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(plugin_id): Path<String>,
    request: Request,
) -> Response {
    if !state.app.plugin_host().hosted() {
        return gate_or_forward(plugins::plugins_on(&state, "DetachPlugin"), &go, request).await;
    }
    match state.app.detach_plugin(&plugin_id).await {
        Ok(()) => empty_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}
