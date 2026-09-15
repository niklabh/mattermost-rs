//! The licence **writes** — `addLicense`, `previewLicense`, `removeLicense` and
//! `requestTrialLicense` (channels/api4/license.go), and `localAddLicense` /
//! `localRemoveLicense` (api4/license_local.go).
//!
//! # A licence lives in the Go process, not in a table
//!
//! `SaveLicense` (platform/license.go:132) validates the bytes, checks the seat count and the
//! expiry, then **`SetLicense`** — an atomic in-memory swap that every `License()` call in the Go
//! server reads — and only then writes `Licenses` and `Systems.ActiveLicenseId`. `RemoveLicense`
//! is the mirror. Nothing re-reads those rows while the server runs: `LoadLicense` is called at
//! start-up and by these two handlers, and by nothing else. So the process that must hold the
//! licence in memory is the process that must save or remove it, exactly as with the
//! configuration (`crate::config_writes`), and the two branches that change what the Go server
//! runs under are forwarded to it:
//!
//! - `addLicense` past a valid signature and a non-trial check — `SaveLicense` and everything in
//!   it (the seat count, the expiry, `SetLicense`, the two rows, the cache flush);
//! - `removeLicense` while a licence is in force — `RemoveLicense` (the row blanked, the swap
//!   to nil, the cache flush).
//!
//! Everything before those points is served: the permission, the multipart parse, the signature
//! check against this process's keys, the trial gate, the whole of `previewLicense` (which
//! saves nothing), `removeLicense` on an unlicensed server (Go returns before touching anything),
//! and `requestTrialLicense`, which on a build without a licence manager is a permission check
//! followed by a 403. On the stack's own server every real licence upload is refused at the
//! signature, since the key its validator trusts is Mattermost's; the licensed oracle
//! (`scripts/go-licensed.sh`) trusts a key pair on disk, and the parity suite signs with it to
//! reach the branches past the signature — `previewLicense`'s 200 and the trial gate's 500 —
//! without ever saving a licence to the shared tables.
//!
//! # `LicenseFromBytes` versus `SaveLicense`'s own validation
//!
//! The HTTP `addLicense` validates **twice**: `LicenseFromBytes` before the trial check, then
//! `SaveLicense` validates the same bytes again. `localAddLicense` skips the first — no trial
//! check, no `LicenseManager` — so its first refusal is `SaveLicense`'s, whose `where` is
//! `addLicense` rather than `LicenseFromBytes`. Both are `NewLicenseValidationAppError`; the
//! JSON decode after a good signature is `api.unmarshal_error` at 500 on the HTTP route and is
//! forwarded on the socket, where it sits behind the signature check inside `SaveLicense`.
//!
//! # What `handlerParamFileAPI` changes, and what is not reproduced
//!
//! Both `POST /license` routes are `FileAPI`, so the body cap is `FileSettings.MaxFileSize +
//! 512` rather than `MaximumPayloadSizeBytes + 512` (web/handlers.go:218). A body past it makes
//! `ParseMultipartForm` fail with a `MaxBytesError`, which `parseLicenseFileFromRequest` wraps
//! and `handleContextError` rewrites to the 413 `api.context.request_body_too_large.app_error`.
//! A hundred-megabyte licence upload is not a case this port reproduces; the cap is not
//! enforced here and such a body is parsed as a form and refused as one.

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use mm_app::license::{LicenseValidationError, license_validation_app_error};
use mm_model::license::License;
use mm_model::permission::{PERMISSION_MANAGE_LICENSE_INFORMATION, make_permission_error};
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::local::{GoLocalSocket, forward_over_unix, partially_migrated};
use crate::multipart::{MultipartError, parse_form};
use crate::proxy;

/// The multipart part both routes read: `r.MultipartForm.File["license"]`.
const LICENSE_PART: &str = "license";

/// `api.license.upgrade_needed.app_error` — a nil `LicenseManager`, which this build always has:
/// a 500 from `addLicense` (license.go:130) and a 403 from `requestTrialLicense` (:227).
pub const UPGRADE_NEEDED_ERROR: &str = "api.license.upgrade_needed.app_error";
/// `api.license.parse_license.parse_form.app_error` — `ParseMultipartForm` failed, HTTP only.
pub const PARSE_FORM_ERROR: &str = "api.license.parse_license.parse_form.app_error";
/// `api.license.add_license.no_file.app_error` — no `license` file part, both routers.
pub const NO_FILE_ERROR: &str = "api.license.add_license.no_file.app_error";

/// `http.ErrNotMultipart` (net/http/request.go:38) — what `localAddLicense` writes as plain
/// text when the request is not multipart at all.
const ERR_NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
/// `http.ErrMissingBoundary` (net/http/request.go:36).
const ERR_MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";

/// `license_local.go:17-18`: the two socket pairs, on the path `local.rs` registers only
/// `/license/client` beneath.
pub(crate) fn local_routes(state: &AppState) -> Router<AppState> {
    let _ = state;
    Router::new().route(
        "/api/v4/license",
        partially_migrated(post(local_add_license).delete(local_remove_license)),
    )
}

/// `SessionHasPermissionToAndNotRestrictedAdmin(PermissionManageLicenseInformation)`, the gate
/// every HTTP handler here opens with.
async fn require_manage_license_information(
    state: &AppState,
    session: &Session,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to_and_not_restricted_admin(
            session,
            &PERMISSION_MANAGE_LICENSE_INFORMATION,
        )
        .await
    {
        Ok(())
    } else {
        Err(ApiError::from(make_permission_error(
            session,
            &[&PERMISSION_MANAGE_LICENSE_INFORMATION],
        )))
    }
}

/// Why the licence part could not be taken from the body.
enum PartFailure {
    /// `ParseMultipartForm` failed — the HTTP routes' `parse_form` 400, the socket's plain text.
    Unparseable(MultipartError),
    /// The form parsed and has no `license` file — `no_file`, both routers.
    NoFile,
}

/// `r.ParseMultipartForm` then `r.MultipartForm.File["license"][0]`, read to bytes.
///
/// `len(fileArray) <= 0` (`array.app_error`) cannot happen — `readForm` never records an empty
/// slice under a name — and `fileData.Open()` / `io.Copy` cannot fail on a buffered form, so
/// their two error ids are unreachable and not reproduced.
fn license_part(parts: &axum::http::request::Parts, bytes: &[u8]) -> Result<Vec<u8>, PartFailure> {
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let form = parse_form(content_type, bytes).map_err(PartFailure::Unparseable)?;
    form.first_file(LICENSE_PART)
        .map(|part| part.data.clone())
        .ok_or(PartFailure::NoFile)
}

/// Port of `parseLicenseFileFromRequest` (license.go:55): every parse failure is one 400, and
/// a missing part is another.
fn parse_license_file_from_request(
    parts: &axum::http::request::Parts,
    bytes: &[u8],
) -> Result<Vec<u8>, ApiError> {
    license_part(parts, bytes).map_err(|failure| match failure {
        PartFailure::Unparseable(err) => {
            tracing::debug!(error = %err, "the licence upload is not a multipart form");
            ApiError::from(AppError::new(
                "parseLicenseFileFromRequest",
                PARSE_FORM_ERROR,
                None,
                err.to_string(),
                400,
            ))
        }
        PartFailure::NoFile => no_file_error("parseLicenseFileFromRequest"),
    })
}

fn no_file_error(where_: &str) -> ApiError {
    ApiError::from(AppError::new(
        where_,
        NO_FILE_ERROR,
        None,
        String::new(),
        400,
    ))
}

fn upgrade_needed_error(where_: &str, status: i32) -> ApiError {
    ApiError::from(AppError::new(
        where_,
        UPGRADE_NEEDED_ERROR,
        None,
        String::new(),
        status,
    ))
}

async fn read_body(
    request: Request,
) -> Result<(axum::http::request::Parts, axum::body::Bytes), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the licence upload body");
            ApiError::from(AppError::new(
                "parseLicenseFileFromRequest",
                PARSE_FORM_ERROR,
                None,
                err.to_string(),
                400,
            ))
        })?;
    Ok((parts, bytes))
}

/// `ReturnStatusOK` — `w.Write(MapToJSON({"status":"OK"}))`, no encoder, no newline.
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

/// `json.NewEncoder(w).Encode(license)` — Go's escaping, then the encoder's newline.
fn encoded_license(license: &License) -> Response {
    match mm_model::utils::go_json_marshal(license) {
        Ok(mut body) => {
            body.push('\n');
            (
                StatusCode::OK,
                [
                    ("Content-Type", "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                body,
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the licence");
            ApiError::from(AppError::new(
                "addLicense",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// Port of `addLicense` (license.go:107): permission, parse, `LicenseFromBytes`, the trial
/// gate — then `SaveLicense`, forwarded (module docs).
///
/// The trial gate: a licence that `IsTrialLicense` but is not `IsSanctionedTrial` asks the
/// `LicenseManager` whether a trial may start, and this build has none — a 500 with the
/// upgrade id (license.go:130), **not** the 403 `requestTrialLicense` gives the same nil.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn add_license(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    if let Err(err) = require_manage_license_information(&state, &session.0).await {
        return err.into_response();
    }
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let signed = match parse_license_file_from_request(&parts, &bytes) {
        Ok(signed) => signed,
        Err(err) => return err.into_response(),
    };
    let license = match state.app.license_from_bytes(&signed) {
        Ok(license) => license,
        Err(err) => {
            return ApiError::from(license_validation_app_error("LicenseFromBytes", &err))
                .into_response();
        }
    };

    if !license.is_sanctioned_trial() && license.is_trial_license() {
        // `c.App.Srv().Platform().LicenseManager()` is nil without the enterprise build.
        return upgrade_needed_error("addLicense", 500).into_response();
    }

    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), Request::from_parts(parts, Body::from(bytes))).await
}

/// Port of `previewLicense` (license.go:181): the parsed licence, encoded, saved nowhere.
///
/// No `Features.SetDefaults` — `LicenseFromBytes` only unmarshals — so a licence file with a
/// sparse `features` object is echoed sparse, `null`s and all.
#[tracing::instrument(skip_all)]
pub async fn preview_license(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_manage_license_information(&state, &session.0).await {
        return err.into_response();
    }
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let signed = match parse_license_file_from_request(&parts, &bytes) {
        Ok(signed) => signed,
        Err(err) => return err.into_response(),
    };
    match state.app.license_from_bytes(&signed) {
        Ok(license) => encoded_license(&license),
        Err(err) => {
            ApiError::from(license_validation_app_error("LicenseFromBytes", &err)).into_response()
        }
    }
}

/// `RemoveLicense` (platform/license.go:310) as far as it can be served: `if license == nil {
/// return nil }` is `ReturnStatusOK` with nothing written; a licence in force is forwarded so
/// the Go process drops its own copy. Shared by both routers; the socket's forward goes over
/// the socket because the request carries [`GoLocalSocket`].
async fn serve_remove_license(state: AppState, request: Request) -> Response {
    match state.app.license().await {
        Ok(None) => {
            tracing::Span::current().record("forwarded", false);
            status_ok()
        }
        Ok(Some(_)) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => {
            tracing::Span::current().record("forwarded", false);
            ApiError::from(err).into_response()
        }
    }
}

/// Port of `removeLicense` (license.go:202): the permission, then [`serve_remove_license`].
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn remove_license(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_manage_license_information(&state, &session.0).await {
        tracing::Span::current().record("forwarded", false);
        return err.into_response();
    }
    serve_remove_license(state, request).await
}

/// Port of `requestTrialLicense` (license.go:218) on a build with no licence manager: the
/// permission, then the 403 `upgrade_needed` — before the body is read, so nothing in it can
/// change the answer. The outbound request to the licence server, and the two `TrialLicense
/// Request` shapes it takes, sit behind that nil and are not reached ([D-702] records what a
/// build with a manager would need).
#[tracing::instrument(skip_all)]
pub async fn request_trial_license(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_manage_license_information(&state, &session.0).await {
        return err.into_response();
    }
    upgrade_needed_error("requestTrialLicense", 403).into_response()
}

// ---------------------------------------------------------------------------------------------
// `license_local.go`
// ---------------------------------------------------------------------------------------------

/// `http.Error(w, err.Error(), http.StatusBadRequest)` — plain text, the message and a newline,
/// `X-Content-Type-Options: nosniff` (which the global headers already carry).
fn plain_text_400(message: &str) -> Response {
    let mut response = (StatusCode::BAD_REQUEST, format!("{message}\n")).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert("x-mmrs-served-by", HeaderValue::from_static("rust"));
    response
}

/// Port of `localAddLicense` (license_local.go:22).
///
/// Three differences from the HTTP route, each measured against Go's socket:
/// - no permission;
/// - a `ParseMultipartForm` failure is **`http.Error`**, plain text with Go's own message,
///   not an `AppError` — the two messages a request can produce before any part is read are
///   reproduced verbatim, and a body that fails *inside* the parse (Go's `multipart: NextPart:
///   EOF` and its kin) is forwarded so the text stays Go's;
/// - no `LicenseFromBytes` and no trial gate: the first check is `SaveLicense`'s own signature
///   validation, refused as `addLicense`, and everything past a good signature — the decode,
///   the nil-features checks, the seat count, the save — is forwarded.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_add_license(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let (parts, bytes) = match read_body(request).await {
        Ok(read) => read,
        Err(err) => return err.into_response(),
    };
    let signed = match license_part(&parts, &bytes) {
        Ok(signed) => signed,
        Err(PartFailure::NoFile) => return no_file_error("addLicense").into_response(),
        Err(PartFailure::Unparseable(MultipartError::NotMultipart)) => {
            return plain_text_400(ERR_NOT_MULTIPART);
        }
        Err(PartFailure::Unparseable(MultipartError::MissingBoundary)) => {
            return plain_text_400(ERR_MISSING_BOUNDARY);
        }
        Err(PartFailure::Unparseable(err)) => {
            tracing::debug!(error = %err, "forwarding a malformed multipart body for Go's own message");
            tracing::Span::current().record("forwarded", true);
            return forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await;
        }
    };
    if let Err(err) = state.app.validate_license_bytes(&signed) {
        // `SaveLicense`'s first line; a `Json` variant cannot come from the validator alone.
        let err: LicenseValidationError = err;
        return ApiError::from(license_validation_app_error("addLicense", &err)).into_response();
    }
    tracing::Span::current().record("forwarded", true);
    forward_over_unix(&go.0, Request::from_parts(parts, Body::from(bytes))).await
}

/// Port of `localRemoveLicense` (license_local.go:92): [`serve_remove_license`] with no
/// permission in front of it.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_remove_license(State(state): State<AppState>, request: Request) -> Response {
    serve_remove_license(state, request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_with(content_type: Option<&str>) -> axum::http::request::Parts {
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v4/license");
        if let Some(content_type) = content_type {
            builder = builder.header("Content-Type", content_type);
        }
        builder.body(()).expect("builds").into_parts().0
    }

    fn form(part_name: &str) -> (String, Vec<u8>) {
        let boundary = "mmrsboundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{part_name}\"; \
             filename=\"license.txt\"\r\n\r\nabc\r\n--{boundary}--\r\n"
        );
        (
            format!("multipart/form-data; boundary={boundary}"),
            body.into_bytes(),
        )
    }

    /// The three outcomes of the shared parse, and the two ids the HTTP wrapper turns them into.
    #[test]
    fn the_license_part_is_taken_by_name_and_its_absence_is_no_file() {
        let (content_type, body) = form("license");
        assert_eq!(
            license_part(&parts_with(Some(&content_type)), &body).ok(),
            Some(b"abc".to_vec())
        );

        let (content_type, body) = form("licence");
        let err =
            parse_license_file_from_request(&parts_with(Some(&content_type)), &body).unwrap_err();
        assert_eq!(err.0.id, NO_FILE_ERROR);
        assert_eq!(err.0.where_, "parseLicenseFileFromRequest");

        let err = parse_license_file_from_request(&parts_with(None), b"").unwrap_err();
        assert_eq!(err.0.id, PARSE_FORM_ERROR);
        assert_eq!(err.0.status_code, 400);
    }

    /// `http.Error` framing: text, a trailing newline, and the two messages Go's `net/http`
    /// produces before a part is read.
    #[test]
    fn the_socket_parse_refusal_is_gos_plain_text() {
        let response = plain_text_400(ERR_NOT_MULTIPART);
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            ERR_NOT_MULTIPART,
            "request Content-Type isn't multipart/form-data"
        );
        assert_eq!(
            ERR_MISSING_BOUNDARY,
            "no multipart boundary param in Content-Type"
        );
    }

    /// The same nil `LicenseManager` is a 500 on one route and a 403 on the other.
    #[test]
    fn upgrade_needed_carries_the_callers_status() {
        assert_eq!(upgrade_needed_error("addLicense", 500).0.status_code, 500);
        assert_eq!(
            upgrade_needed_error("requestTrialLicense", 403)
                .0
                .status_code,
            403
        );
        assert_eq!(upgrade_needed_error("x", 403).0.id, UPGRADE_NEEDED_ERROR);
    }
}
