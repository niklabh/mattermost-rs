//! The **miscellaneous** local-mode families: thirty-one `*_local.go` registrations whose HTTP
//! handler is already ported, on the unix socket.
//!
//! `job_local.go` (7), `preference_local.go` (5), `custom_profile_attributes_local.go` (7),
//! `export_local.go` (4), `import_local.go` (2), the last pair of `bot_local.go`
//! (`convert_to_user`), `getUpload` from `upload_local.go`, `generateSupportPacket` from
//! `system_local.go`, `getLdapGroups` from `ldap_local.go`, and the two reads of
//! `config_local.go`. See [`crate::local`] for the transport and the authentication model; what
//! this module adds is the wiring, and three things the wiring has to get right:
//!
//! # 1. Every forward goes over the socket
//!
//! Several of these handlers hand a request to Go from *inside* the handler — an
//! access-control job, a `flagged_post` preference, a `?remove_masked=` config, a non-local
//! file backend, a segment outside gorilla's charset. On the HTTP router that forward dials the
//! port; on this one it must dial Go's **socket**, or the request reaches `APISessionRequired`
//! and is refused where Go's local mux answers it. So each such handler is called through its
//! transport-free core (`*_from_body`, `*_inner`, `*_answer`), and `None` is forwarded with
//! [`local::forward_over_unix`]. Where the core is not split out — the two preference reads with
//! a `{category}`, the by-type job listing — the wrapper runs the same charset check first, and
//! the handler's own port forward is unreachable behind it, as [`crate::local`] does for
//! `role_name`.
//!
//! # 2. `me` is nobody
//!
//! The socket's session has no user, so every `{user_id}` path given `me` — five of the
//! preference and CPA pairs — is a 400 naming `user_id`, and `PATCH /custom_profile_attributes
//! /values`, which has no path segment and reads the session's user, reaches the value patch
//! with an **empty target**. Measured against Go's socket: the fields are read before the
//! target is checked, so an unlicensed server answers the 404 `property_field.not_found` for a
//! made-up id and never writes. No local-only branch reproduces any of this; the handlers do
//! the rewrite from the session they are handed.
//!
//! # 3. Two of the handlers are *not* the HTTP ones
//!
//! `localGetConfig` and `localGetClientConfig` (config_local.go) are separate functions, and the
//! difference is the local session's privilege written as **missing code**: no permission check,
//! no `readFilter` merge, no cloud tag filter, and — the part a wrong guess would leak or hide —
//! `c.App.Config()` rather than `GetSanitizedConfig()`, so the socket sees the database password
//! and every other secret the HTTP route masks. See [`local_get_config`] and
//! [`local_get_client_config`].

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, RawQuery, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};

use crate::error::ApiError;
use crate::local::{
    GoLocalSocket, forward_over_unix, local_session, partially_migrated,
    partially_migrated_with_ids,
};
use crate::{
    AppState, config, custom_profile_attributes as cpa, exports, gated_reads, jobs, preferences,
    uploads, user_convert,
};

/// The thirty-one registrations, merged into [`crate::local::router`].
pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        // ---- `job_local.go`, all seven. `{job_id:[A-Za-z0-9]+}` is id-shaped, so the id
        // middleware forwards a segment outside the class; `{job_type:[A-Za-z0-9_-]+}` is not,
        // and its wrapper carries the check.
        .route(
            "/api/v4/jobs",
            partially_migrated(get(local_get_jobs).post(local_create_job)),
        )
        .route(
            "/api/v4/jobs/type/{job_type}",
            partially_migrated(get(local_get_jobs_by_type)),
        )
        .route(
            "/api/v4/jobs/{job_id}",
            partially_migrated_with_ids(state, get(local_get_job)),
        )
        .route(
            "/api/v4/jobs/{job_id}/download",
            partially_migrated_with_ids(state, get(local_download_job)),
        )
        .route(
            "/api/v4/jobs/{job_id}/cancel",
            partially_migrated_with_ids(state, post(local_cancel_job)),
        )
        .route(
            "/api/v4/jobs/{job_id}/status",
            partially_migrated_with_ids(state, patch(local_update_job_status)),
        )
        // ---- `preference_local.go`, all five. The `PUT` on `{user_id}` is registered here and
        // not on the HTTP router, which serves it as `/users/me/preferences` only; the handler
        // is shared, see `preferences::update_preferences_for`.
        .route(
            "/api/v4/users/{user_id}/preferences",
            partially_migrated_with_ids(
                state,
                get(local_get_preferences).put(local_update_preferences),
            ),
        )
        // The literal sibling of `{category}`, for the reason `lib.rs` gives: gorilla registers
        // `/delete` for POST alone and lets a GET fall through to the category route; axum does
        // not fall through, so the literal answers the GET too.
        .route(
            "/api/v4/users/{user_id}/preferences/delete",
            partially_migrated_with_ids(
                state,
                post(local_delete_preferences).get(local_get_preferences_named_delete),
            ),
        )
        .route(
            "/api/v4/users/{user_id}/preferences/{category}",
            partially_migrated_with_ids(state, get(local_get_preferences_by_category)),
        )
        .route(
            "/api/v4/users/{user_id}/preferences/{category}/name/{preference_name}",
            partially_migrated_with_ids(state, get(local_get_preference_by_category_and_name)),
        )
        // ---- `custom_profile_attributes_local.go`, all seven.
        .route(
            "/api/v4/custom_profile_attributes/fields",
            partially_migrated(get(local_list_cpa_fields).post(local_create_cpa_field)),
        )
        .route(
            "/api/v4/custom_profile_attributes/fields/{field_id}",
            partially_migrated_with_ids(
                state,
                patch(local_patch_cpa_field).delete(local_delete_cpa_field),
            ),
        )
        .route(
            "/api/v4/custom_profile_attributes/values",
            partially_migrated(patch(local_patch_cpa_values)),
        )
        .route(
            "/api/v4/users/{user_id}/custom_profile_attributes",
            partially_migrated_with_ids(
                state,
                get(local_list_cpa_values).patch(local_patch_cpa_values_for_user),
            ),
        )
        // ---- `export_local.go` (four) and `import_local.go` (two). `{export_name:.+\.zip}` is
        // not id-shaped and the wrappers' cores carry the check.
        .route(
            "/api/v4/exports",
            partially_migrated(get(local_list_exports)),
        )
        .route(
            "/api/v4/exports/{export_name}",
            partially_migrated(get(local_download_export).delete(local_delete_export)),
        )
        .route(
            "/api/v4/exports/{export_name}/presign-url",
            partially_migrated(post(local_generate_presign_url_export)),
        )
        .route(
            "/api/v4/imports",
            partially_migrated(get(local_list_imports)),
        )
        .route(
            "/api/v4/imports/{import_name}",
            partially_migrated(delete(local_delete_import)),
        )
        // ---- `bot_local.go`'s seventh pair; the other six are in `local.rs`. A new path, so
        // nothing there needs a method added.
        .route(
            "/api/v4/bots/{bot_user_id}/convert_to_user",
            partially_migrated_with_ids(state, post(local_convert_bot_to_user)),
        )
        // ---- `upload_local.go:10`. The two `POST`s beside it (`createUpload`, `uploadData`)
        // are another family's; the method fallback forwards them.
        .route(
            "/api/v4/uploads/{upload_id}",
            partially_migrated_with_ids(state, get(local_get_upload)),
        )
        // ---- `system_local.go:20` and `ldap_local.go:12`, each served up to its licence gate
        // exactly as on the HTTP router.
        .route(
            "/api/v4/system/support_packet",
            partially_migrated(get(local_generate_support_packet)),
        )
        .route(
            "/api/v4/ldap/groups",
            partially_migrated(get(local_get_ldap_groups)),
        )
        // ---- `config_local.go:19` and `:24` — the two `local*` handlers. The `PUT`, `/patch`,
        // `/reload` and `/migrate` pairs fall to the fallbacks.
        .route("/api/v4/config", partially_migrated(get(local_get_config)))
        .route(
            "/api/v4/config/client",
            partially_migrated(get(local_get_client_config)),
        )
}

// ---------------------------------------------------------------------------------------------
// jobs
// ---------------------------------------------------------------------------------------------

/// `getJobs` through `APILocal` (job_local.go:10).
///
/// `SessionHasPermissionToReadJob` short-circuits on the local session for every type, so the
/// socket lists every job — which is what `mmctl job list` relies on.
async fn local_get_jobs(state: State<AppState>, query: RawQuery) -> Result<Response, ApiError> {
    jobs::get_jobs(state, query, local_session()).await
}

/// `createJob` through `APILocal` (job_local.go:11).
///
/// An access-control sync job is handed to Go over the **socket**: Go's handler stamps
/// `requester_id` with the session's user id, which is empty here, and that is Go's answer to
/// give.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_create_job(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("job").into_response();
        }
    };
    match jobs::create_job_from_body(&state, &local_session(), &bytes).await {
        Some(response) => response,
        None => forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await,
    }
}

/// `getJob` through `APILocal` (job_local.go:12).
async fn local_get_job(state: State<AppState>, path: Path<String>) -> Result<Response, ApiError> {
    jobs::get_job(state, path, local_session()).await
}

/// `downloadJob` through `APILocal` (job_local.go:13). With `DownloadExportResults` on, the
/// export filestore is Go's, over the socket.
async fn local_download_job(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(job_id): Path<String>,
    request: Request,
) -> Response {
    match gated_reads::download_job_answer(&state, &job_id) {
        Some(response) => response,
        None => forward_over_unix(&go.0, request).await,
    }
}

/// `cancelJob` through `APILocal` (job_local.go:14).
async fn local_cancel_job(state: State<AppState>, path: Path<String>) -> Response {
    jobs::cancel_job(state, path, local_session()).await
}

/// `updateJobStatus` through `APILocal` (job_local.go:16).
async fn local_update_job_status(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    jobs::update_job_status(state, path, local_session(), request).await
}

/// `getJobsByType` through `APILocal` (job_local.go:15).
///
/// `{job_type:[A-Za-z0-9_-]+}` is not id-shaped, so the id middleware does not see it; the
/// check runs here and a miss is forwarded over the socket for Go's own mux 404 — the HTTP
/// handler's port forward is unreachable behind it.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_get_jobs_by_type(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(job_type): Path<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    if !jobs::segment_matches_job_type_mux(&job_type) {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }
    tracing::Span::current().record("forwarded", false);
    match jobs::get_jobs_by_type_inner(state, job_type, query, local_session()).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// preferences
// ---------------------------------------------------------------------------------------------

/// `getPreferences` through `APILocal` (preference_local.go:7). `me` is a 400 here.
async fn local_get_preferences(
    state: State<AppState>,
    path: Path<String>,
) -> Result<Response, ApiError> {
    preferences::get_preferences(state, path, local_session()).await
}

/// `updatePreferences` through `APILocal` (preference_local.go:8).
///
/// The one pair in this module the HTTP router does not register under this path. A batch with
/// a `flagged_post`, `direct_channel_show` or `group_channel_show` entry is Go's over the socket.
#[tracing::instrument(skip_all, fields(user_id = %user_id, count, forwarded))]
async fn local_update_preferences(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(user_id): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("preferences").into_response();
        }
    };
    match preferences::update_preferences_for(&state, &local_session(), &user_id, &bytes).await {
        Some(response) => response,
        None => forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await,
    }
}

/// `deletePreferences` through `APILocal` (preference_local.go:9).
async fn local_delete_preferences(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    preferences::delete_preferences(state, local_session(), path, request).await
}

/// `GET /users/{user_id}/preferences/delete` — `getPreferencesByCategory` with `delete` as the
/// category, as on the HTTP router; the segment is inside the mux class so nothing forwards.
async fn local_get_preferences_named_delete(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    preferences::get_preferences_named_delete(state, path, local_session(), request).await
}

/// `getPreferencesByCategory` through `APILocal` (preference_local.go:10).
///
/// `{category:[A-Za-z0-9_]+}` is not id-shaped; the check runs here, the miss is forwarded over
/// the socket, and the handler's own port forward is unreachable.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_get_preferences_by_category(
    state: State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path((user_id, category)): Path<(String, String)>,
    request: Request,
) -> Response {
    if !preferences::segment_matches_preference_mux(&category) {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }
    tracing::Span::current().record("forwarded", false);
    preferences::get_preferences_by_category(
        state,
        Path((user_id, category)),
        local_session(),
        request,
    )
    .await
}

/// `getPreferenceByCategoryAndName` through `APILocal` (preference_local.go:11). Both segments
/// carry the `[A-Za-z0-9_]+` class.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_get_preference_by_category_and_name(
    state: State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path((user_id, category, preference_name)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    if !preferences::segment_matches_preference_mux(&category)
        || !preferences::segment_matches_preference_mux(&preference_name)
    {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }
    tracing::Span::current().record("forwarded", false);
    preferences::get_preference_by_category_and_name(
        state,
        Path((user_id, category, preference_name)),
        local_session(),
        request,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// custom profile attributes
// ---------------------------------------------------------------------------------------------

/// `listCPAFields` through `APILocal` (custom_profile_attributes_local.go:10).
async fn local_list_cpa_fields(state: State<AppState>) -> Response {
    cpa::list_cpa_fields(state, local_session()).await
}

/// `createCPAField` through `APILocal` (:11).
async fn local_create_cpa_field(state: State<AppState>, request: Request) -> Response {
    cpa::create_cpa_field(state, local_session(), request).await
}

/// `patchCPAField` through `APILocal` (:12).
async fn local_patch_cpa_field(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    cpa::patch_cpa_field(state, path, local_session(), request).await
}

/// `deleteCPAField` through `APILocal` (:13).
async fn local_delete_cpa_field(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    cpa::delete_cpa_field(state, path, local_session(), request).await
}

/// `listCPAValues` through `APILocal` (:14).
///
/// The handler's one forward — `hasTargetAccess`'s `UserCanSeeOtherUser` arm — is unreachable
/// here: an unrestricted session passes the target check before that lookup, in Go and in the
/// port alike. `me` is a 400.
async fn local_list_cpa_values(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    cpa::list_cpa_values(state, path, local_session(), request).await
}

/// `patchCPAValues` through `APILocal` (:15) — the session's user, which is **nobody**; see the
/// module docs for what Go does with an empty target.
async fn local_patch_cpa_values(state: State<AppState>, request: Request) -> Response {
    cpa::patch_cpa_values(state, local_session(), request).await
}

/// `patchCPAValuesForUser` through `APILocal` (:16).
async fn local_patch_cpa_values_for_user(
    state: State<AppState>,
    path: Path<String>,
    request: Request,
) -> Response {
    cpa::patch_cpa_values_for_user(state, path, local_session(), request).await
}

// ---------------------------------------------------------------------------------------------
// exports and imports
// ---------------------------------------------------------------------------------------------

/// `listExports` through `APILocal` (export_local.go:10).
async fn local_list_exports(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    match exports::list_exports_inner(&state, &local_session()).await {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `listImports` through `APILocal` (import_local.go:10).
async fn local_list_imports(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    match exports::list_imports_inner(&state, &local_session()).await {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `downloadExport` through `APILocal` (export_local.go:12).
async fn local_download_export(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(export_name): Path<String>,
    request: Request,
) -> Response {
    // See `files::serve_bytes` on why the method and headers are cloned rather than borrowed.
    let method = request.method().clone();
    let headers = request.headers().clone();
    match exports::download_export_inner(&state, &export_name, &local_session(), &method, &headers)
        .await
    {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `deleteExport` through `APILocal` (export_local.go:11).
async fn local_delete_export(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(export_name): Path<String>,
    request: Request,
) -> Response {
    match exports::delete_archive(
        &state,
        &local_session(),
        &export_name,
        exports::Archive::Export,
    )
    .await
    {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `deleteImport` through `APILocal` (import_local.go:11).
async fn local_delete_import(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(import_name): Path<String>,
    request: Request,
) -> Response {
    match exports::delete_archive(
        &state,
        &local_session(),
        &import_name,
        exports::Archive::Import,
    )
    .await
    {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `generatePresignURLExport` through `APILocal` (export_local.go:13).
async fn local_generate_presign_url_export(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Path(export_name): Path<String>,
    request: Request,
) -> Response {
    match exports::generate_presign_url_export_inner(&state, &local_session(), &export_name).await {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// the singletons
// ---------------------------------------------------------------------------------------------

/// `convertBotToUser` through `APILocal` (bot_local.go:13).
async fn local_convert_bot_to_user(
    state: State<AppState>,
    path: Path<String>,
    query: RawQuery,
    request: Request,
) -> Response {
    user_convert::convert_bot_to_user(state, path, query, local_session(), request).await
}

/// `getUpload` through `APILocal` (upload_local.go:10).
///
/// `upload.UserId != session.UserId && !IsSystemAdmin()` — the session's user is empty, so the
/// first half is true for every upload and the second is the `manage_system` shortcut. The socket
/// reads any upload session.
async fn local_get_upload(state: State<AppState>, path: Path<String>) -> Response {
    uploads::get_upload(state, path, local_session()).await
}

/// `generateSupportPacket` through `APILocal` (system_local.go:20), up to the licence gate as on
/// the HTTP router; a licensed server's packet is Go's, over the socket.
async fn local_generate_support_packet(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    match gated_reads::support_packet_answer(&state, &local_session()).await {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// `getLdapGroups` through `APILocal` (ldap_local.go:12) — the two 501s, as on the HTTP router.
async fn local_get_ldap_groups(state: State<AppState>, request: Request) -> Response {
    gated_reads::get_ldap_groups(state, local_session(), request).await
}

// ---------------------------------------------------------------------------------------------
// the two `local*` config handlers
// ---------------------------------------------------------------------------------------------

/// Port of `localGetConfig` (api4/config_local.go:29).
///
/// # Not `getConfig`, and the difference is what the socket is allowed to see
///
/// `getConfig` (config.go:52) checks `SysconsoleReadPermissions`, merges the config through
/// `readFilter`, adds a `cloud_restrictable` tag filter on a cloud licence, and — the line that
/// matters — starts from `GetSanitizedConfig()`. `localGetConfig` has **none of that**: it hands
/// `c.App.Config()` straight to `FilterConfig`, so the body carries `SqlSettings.DataSource`
/// with the database password in it, `AtRestEncryptKey`, `PublicLinkSalt` and the rest, where
/// the HTTP route writes `********************************`. Measured against Go's socket. This
/// is the local session's privilege expressed as missing code, and reusing the HTTP handler
/// would have masked six values Go shows.
///
/// What is shared: `?remove_masked=` / `?remove_defaults=` are read the same way and, as on the
/// HTTP router, forwarded ([D-313]) — over the socket; `FilterConfig` with neither option is
/// `cfg.StringMap()`, a struct-to-map, which `serde_json::to_value` of the loaded struct is; and
/// the response is `json.NewEncoder` (newline) under `Cache-Control: no-cache, no-store,
/// must-revalidate`.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_get_config(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    if config::filtered_config_requested(request.uri().query()) {
        tracing::Span::current().record("forwarded", true);
        return forward_over_unix(&go.0, request).await;
    }
    tracing::Span::current().record("forwarded", false);

    // `c.App.Config()` — **not** sanitized. The one line that separates this from `getConfig`.
    let config = match mm_app::config::load_model_config(state.app.store().config()).await {
        Ok(config) => config,
        Err(err) => return config::config_error("getConfig", &err).into_response(),
    };

    match serde_json::to_value(&config) {
        Ok(body) => config::json_response(&body, true, true),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the configuration");
            ApiError::from(mm_model::utils::AppError::new(
                "getConfig",
                "api.filter_config_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// Port of `localGetClientConfig` (api4/config_local.go:224).
///
/// `getClientConfig` (config.go:253) picks the limited map when `Session().UserId == ""` — which
/// on this transport is always, so reusing it would answer the **anonymous** map to root.
/// `localGetClientConfig` has no branch: `ClientConfigWithComputed()`, the full map, every time.
/// And it is `w.Write(MapToJSON(...))` rather than an encoder, so **no trailing newline** — the
/// HTTP route's body ends in one — and, like the HTTP route, no `Cache-Control`.
#[tracing::instrument(skip_all, fields(keys))]
async fn local_get_client_config(State(state): State<AppState>) -> Response {
    let props = match state.app.client_config_with_computed().await {
        Ok(props) => props,
        Err(err) => return config::config_error("localGetClientConfig", &err).into_response(),
    };
    tracing::Span::current().record("keys", props.len());
    match serde_json::to_value(&props) {
        Ok(body) => config::json_response(&body, false, false),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the client configuration");
            ApiError::from(mm_model::utils::AppError::new(
                "localGetClientConfig",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// axum reports an overlapping path when the router is **built**, not when it is served —
    /// so building [`crate::local::router`], which merges this module's routes into its own,
    /// is the test that no path here is also registered there. `connect_lazy` needs a reactor
    /// but never opens a connection.
    #[tokio::test]
    async fn the_merge_adds_no_path_the_local_router_already_has() {
        let app = mm_app::App::new(mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://x/y")
                .expect("a lazy pool needs no server"),
        ));
        let state = AppState::new(app, "http://localhost:1".to_owned());
        let _router =
            crate::local::router(state, std::path::PathBuf::from("/nonexistent/go.socket"));
    }

    /// One wrapper per registered pair, plus the literal `/preferences/delete` GET that axum
    /// needs and Go does not — so a wrapper dropped by a merge conflict is a diff here rather
    /// than a route silently forwarded.
    #[test]
    fn there_is_one_wrapper_per_pair_plus_the_delete_get() {
        let wrappers = include_str!("local_misc.rs")
            .lines()
            .filter(|line| line.trim_start().starts_with("async fn local_"))
            .count();
        assert_eq!(wrappers, 31 + 1);
    }
}
