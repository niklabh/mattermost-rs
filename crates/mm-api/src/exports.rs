//! The four export routes and the two import ones — `api4/export.go` and `api4/import.go`.
//!
//! Six handlers over two directories, all gated on `manage_system`, and every one of them differs
//! from its opposite number in a way that is not a pattern:
//!
//! | | exports | imports |
//! |---|---|---|
//! | backend | the **export** backend | the **file** backend |
//! | listing filter | none — a `.tmp` export is listed | `.tmp` entries are dropped |
//! | list body | `json.Marshal` + `Write` — **no trailing newline** | `json.NewEncoder().Encode` — **with one** |
//! | delete of a missing name | `{"status":"OK"}` | `{"status":"OK"}` |
//! | download | `GET /exports/{name}` | none |
//!
//! The newline is the one a client would actually notice, and it comes from two different
//! spellings of "write this as JSON" in two files by two authors. [D-086] is the standing entry
//! for that distinction.
//!
//! # The route pattern is `{export_name:.+\.zip}` and it is a `PathPrefix`
//!
//! So `export_name` may contain slashes in Go. axum's `{export_name}` matches a single segment,
//! and a multi-segment name simply does not match this router — the request falls through to the
//! proxy and Go answers it. The `.zip` suffix is enforced here for the same reason
//! `segment_matches_go_mux` exists: a name without it is a route Go's mux never matched, so it
//! must reach Go's own 404 rather than one of ours.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_model::go_path;
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;
use crate::serve_content::{FileResponse, serve_content_with_headers};

/// The suffix gorilla requires of both `{export_name}` and `{import_name}`.
const ARCHIVE_SUFFIX: &str = ".zip";

/// `c.IsSystemAdmin()` (web/context.go:134) — which is exactly
/// `SessionHasPermissionTo(manage_system)`, not a role test. `deleteImport` spells the same check
/// out longhand; the two are the same call.
async fn require_system_admin(
    state: &AppState,
    session: &AuthenticatedSession,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        &session.0,
        &[&PERMISSION_MANAGE_SYSTEM],
    )))
}

/// `{export_name:.+\.zip}` — a name gorilla would not have routed forwards instead of answering.
fn archive_name_is_routable(name: &str) -> bool {
    // `.+\.zip` needs at least one character before the suffix, so `.zip` alone does not match.
    name.len() > ARCHIVE_SUFFIX.len() && name.ends_with(ARCHIVE_SUFFIX)
}

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

/// Port of `listExports` (api4/export.go:23), reached as `GET /api/v4/exports`.
///
/// **No trailing newline**: `json.Marshal` then `w.Write`, unlike its import counterpart.
/// The body is always an array — `ListExports` builds it with `make([]string, len(exports))`, so
/// an empty export directory is `[]` and never `null`.
#[tracing::instrument(skip_all)]
pub async fn list_exports(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match list_exports_inner(&state, &session).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn list_exports_inner(
    state: &AppState,
    session: &AuthenticatedSession,
) -> Result<Option<Response>, ApiError> {
    require_system_admin(state, session).await?;

    let exports = match state.app.list_exports().await {
        Ok(exports) => exports,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };

    Ok(Some(json_array_without_newline(&exports, "listExports")?))
}

/// Port of `listImports` (api4/import.go:19), reached as `GET /api/v4/imports`.
///
/// **With** a trailing newline, and the list is built with `make([]string, 0, len(imports))` —
/// so an empty import directory is also `[]` rather than `null`, by a different construction.
#[tracing::instrument(skip_all)]
pub async fn list_imports(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match list_imports_inner(&state, &session).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn list_imports_inner(
    state: &AppState,
    session: &AuthenticatedSession,
) -> Result<Option<Response>, ApiError> {
    require_system_admin(state, session).await?;

    let imports = match state.app.list_imports().await {
        Ok(imports) => imports,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };

    let mut body =
        serde_json::to_vec(&imports).map_err(|err| marshal_error(&err, "listImports"))?;
    body.push(b'\n');
    Ok(Some(
        (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    ))
}

fn json_array_without_newline(
    values: &[String],
    where_: &'static str,
) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(values).map_err(|err| marshal_error(&err, where_))?;
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

fn marshal_error(err: &serde_json::Error, where_: &'static str) -> ApiError {
    tracing::error!(error = %err, "failed to serialise the listing");
    // `listExports`' marshal failure is raised with the `where` **`listImports`** — Go's
    // copy-paste (api4/export.go:37), reproduced.
    let _ = where_;
    ApiError::from(*AppError::boxed(
        "listImports",
        "app.export.marshal.app_error",
        None,
        String::new(),
        500,
    ))
}

/// Port of `deleteExport` (api4/export.go:45), reached as
/// `DELETE /api/v4/exports/{export_name}`.
///
/// **Deleting a name that is not there succeeds.** `App.DeleteExport` returns `nil` when the file
/// does not exist, so this answers `{"status":"OK"}` — there is no 404 on this route at all.
#[tracing::instrument(skip_all, fields(export_name = %export_name))]
pub async fn delete_export(
    State(state): State<AppState>,
    Path(export_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match delete_archive(&state, &session, &export_name, Archive::Export).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

/// Port of `deleteImport` (api4/import.go:36), reached as
/// `DELETE /api/v4/imports/{import_name}`.
#[tracing::instrument(skip_all, fields(import_name = %import_name))]
pub async fn delete_import(
    State(state): State<AppState>,
    Path(import_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match delete_archive(&state, &session, &import_name, Archive::Import).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

#[derive(Clone, Copy)]
enum Archive {
    Export,
    Import,
}

async fn delete_archive(
    state: &AppState,
    session: &AuthenticatedSession,
    name: &str,
    which: Archive,
) -> Result<Option<Response>, ApiError> {
    if !archive_name_is_routable(name) {
        tracing::debug!(name, "outside the router's `.+\\.zip` pattern; forwarding");
        return Ok(None);
    }

    require_system_admin(state, session).await?;

    let result = match which {
        Archive::Export => state.app.delete_export(name).await,
        Archive::Import => state.app.delete_import(name).await,
    };

    match result {
        Ok(()) => Ok(Some(status_ok())),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, name, "forwarding to Go");
            Ok(None)
        }
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
    }
}

/// Port of `downloadExport` (api4/export.go:64), reached as `GET /api/v4/exports/{export_name}`.
///
/// # It does **not** use `WriteFileResponse`
///
/// One header of its own — `Content-Type: application/zip` — and then `http.ServeContent` with
/// the **zero** `time.Time`. So the response carries no `Cache-Control`, no
/// `Content-Disposition`, no `X-Frame-Options` and no `Last-Modified`; a browser opening this URL
/// gets a zip with no suggested filename. Range requests still work, because they are
/// `ServeContent`'s and not Mattermost's.
///
/// # The path it checks and the path it opens are the same, and both are pre-joined
///
/// `filepath.Join(*ExportSettings.Directory, name)` is computed in the handler and handed to the
/// backend, which joins its **own** directory on the front. So a stock server looks under
/// `./data/` + `export/<name>`. Reading `FileSettings.ExportDirectory` for the first half instead
/// would land in `./data/data/<name>` — plausible, wrong, and silent.
#[tracing::instrument(skip_all, fields(export_name = %export_name))]
pub async fn download_export(
    State(state): State<AppState>,
    Path(export_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // See `files::serve_bytes` on why the method and headers are cloned rather than borrowed.
    let method = request.method().clone();
    let headers = request.headers().clone();

    match download_export_inner(&state, &export_name, &session, &method, &headers).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn download_export_inner(
    state: &AppState,
    export_name: &str,
    session: &AuthenticatedSession,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> Result<Option<Response>, ApiError> {
    if !archive_name_is_routable(export_name) {
        tracing::debug!(
            export_name,
            "outside the router's `.+\\.zip` pattern; forwarding"
        );
        return Ok(None);
    }

    require_system_admin(state, session).await?;

    let path = go_path::join(&[&state.app.config().export_directory, export_name]);

    let exists = match state.app.export_file_exists(&path).await {
        Ok(exists) => exists,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, export_name, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };
    if !exists {
        return Err(ApiError::from(*AppError::boxed(
            "downloadExport",
            "api.export.export_not_found.app_error",
            None,
            String::new(),
            404,
        )));
    }

    let (file, size) = match state.app.export_file_reader(&path).await {
        Ok(open) => open,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, export_name, "forwarding to Go");
            return Ok(None);
        }
        // Unlike the file routes, this one does **not** rewrite the status: `ExportFileReader`'s
        // 500 reaches the client as a 500.
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/zip"),
    );

    match serve_content_with_headers(
        response_headers,
        export_name,
        // `time.Time{}` — the zero time, which suppresses `Last-Modified` *and* makes every
        // `If-Range` condFalse.
        None,
        method,
        headers,
        file,
        size,
    )
    .await
    {
        FileResponse::Response(response) => Ok(Some(response)),
        FileResponse::Forward(reason) => {
            tracing::debug!(reason, export_name, "forwarding to Go");
            Ok(None)
        }
    }
}

/// Port of `generatePresignURLExport` (api4/export.go:87), reached as
/// `POST /api/v4/exports/{export_name}/presign-url`.
///
/// **Always a 500 on this deployment**, and at the first of three gates:
/// `FeatureFlags.EnableExportDirectDownload` is `false` and unconfigurable on a Team Edition
/// server. See [`mm_app::App::generate_presign_url_for_export`] for the two behind it, one of
/// which a local file backend could never pass.
///
/// Ported rather than left to the proxy because "this route always refuses, and here is exactly
/// which refusal" is a thing worth pinning: the id carries Go's `eport` typo, and a client
/// matching on it must see the same string from either server.
#[tracing::instrument(skip_all, fields(export_name = %export_name))]
pub async fn generate_presign_url_export(
    State(state): State<AppState>,
    Path(export_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !archive_name_is_routable(&export_name) {
        tracing::debug!(
            export_name,
            "outside the router's `.+\\.zip` pattern; forwarding"
        );
        return proxy::forward_to_go(State(state), request).await;
    }

    if let Err(err) = require_system_admin(&state, &session).await {
        return err.into_response();
    }

    match state
        .app
        .generate_presign_url_for_export(&export_name)
        .await
    {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(body) => (
                StatusCode::OK,
                [
                    ("Content-Type", "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                body,
            )
                .into_response(),
            Err(err) => marshal_error(&err, "generatePresignURLExport").into_response(),
        },
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, export_name, "forwarding to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(PrepareError::App(err)) => ApiError::from(*err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::archive_name_is_routable;

    /// `{export_name:.+\.zip}` needs **at least one character** before the suffix, so `.zip`
    /// alone is not a match and must be forwarded for Go's mux to 404 it.
    #[test]
    fn only_names_gorilla_would_route_are_served() {
        assert!(archive_name_is_routable("job.zip"));
        assert!(archive_name_is_routable("a.zip"));
        assert!(archive_name_is_routable("2026-09-09_export.zip"));

        assert!(!archive_name_is_routable(".zip"));
        assert!(
            !archive_name_is_routable("job.ZIP"),
            "the pattern is case-sensitive"
        );
        assert!(!archive_name_is_routable("job.zip.tmp"));
        assert!(!archive_name_is_routable("job"));
        assert!(!archive_name_is_routable(""));
    }
}
