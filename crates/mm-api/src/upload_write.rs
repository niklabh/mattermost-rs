//! The two write halves of the resumable upload: `createUpload` (api4/upload.go:26,
//! `POST /api/v4/uploads`) and `uploadData` (api4/upload.go:122,
//! `POST /api/v4/uploads/{upload_id}`).
//!
//! The read halves — `getUpload` and `getUploadsForUser` — are in [`crate::uploads`]; this is the
//! bytes side, and the two share nothing but the model.
//!
//! # Where `uploadData` hands the request to Go
//!
//! An image chunk that *completes* the upload is where Go derives a `_preview` and `_thumb`, and
//! that pixel work is deferred ([D-380]/[D-411]) — see [`mm_app::App::upload_data`], which refuses
//! it as `Unreproducible` **before writing the chunk**, so forwarding replays the same request Go
//! would have handled. Every other outcome — the 204 for an incomplete chunk, the completed
//! non-image `FileInfo`, the size and offset refusals, the disabled-attachments 501 — is served.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_UPLOAD_FILE, make_permission_error,
};
use mm_model::upload_session::{UploadSession, UploadType};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::images::{BYTES_MIN_READ, declared_content_length};
use crate::proxy;
use mm_app::post::PrepareError;

/// Port of `createUpload` (api4/upload.go:26), reached as `POST /api/v4/uploads`.
///
/// # A resumable upload is bookkeeping first, bytes later
///
/// This creates the `UploadSessions` row that [`upload_data`] then writes into. In order:
/// `EnableFileAttachments` off is **501**; the body is a `model.UploadSession` (a decode failure
/// is the 400 `upload` invalid-param); `RemoteId`/`ReqFileId` are cleared (shared-channels only);
/// the filename is reduced to its base. Then the two `Type` branches — `import` needs
/// `manage_system` and refuses on a cloud licence and on a plugin/import directory conflict;
/// anything else is forced to `attachment` and needs `upload_file` on the channel. Finally the id
/// and user are stamped, `FileSize > MaxFileSize` is the **413**, and
/// [`mm_app::App::create_upload_session`] validates, checks the channel and saves.
///
/// Success is **201** with the session JSON and a trailing newline (`json.NewEncoder.Encode`).
#[tracing::instrument(skip_all, fields(upload_type, forwarded = false))]
pub async fn create_upload(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match serve_create_upload(&state, &session, &parts, body).await {
        Ok(response) => response,
        Err(Refusal::Api(err)) => err.into_response(),
        Err(Refusal::Forward(bytes)) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// A refusal, or a hand-over to Go carrying the body to replay.
enum Refusal {
    Api(ApiError),
    Forward(axum::body::Bytes),
}

impl From<ApiError> for Refusal {
    fn from(err: ApiError) -> Self {
        Refusal::Api(err)
    }
}

impl From<Box<AppError>> for Refusal {
    fn from(err: Box<AppError>) -> Self {
        Refusal::Api(ApiError::from(*err))
    }
}

async fn serve_create_upload(
    state: &AppState,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<Response, Refusal> {
    if !state.app.config().file_enable_file_attachments {
        return Err(attachments_disabled("createUpload", 501).into());
    }

    let bytes = read_capped_body(state, parts, body).await?;

    let mut us: UploadSession = serde_json::from_slice(&bytes).map_err(|err| {
        tracing::warn!(error = %err, "the upload session body will not decode");
        Refusal::Api(ApiError::invalid_param("upload"))
    })?;

    // Not supported for client uploads; shared channels only.
    us.remote_id = String::new();
    us.req_file_id = String::new();
    us.filename = mm_model::go_path::base(&us.filename);

    if us.type_.as_str() == UploadType::IMPORT {
        // `c.IsSystemAdmin()` is `manage_system`.
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
        {
            return Err(make_permission_error(&session.0, &[&PERMISSION_MANAGE_SYSTEM]).into());
        }
        // `License().IsCloud()` — false on this stack; a cloud licence is the 400 below.
        if state
            .app
            .license()
            .await
            .ok()
            .flatten()
            .is_some_and(|license| license.is_cloud())
        {
            return Err(cloud_upload("createUpload").into());
        }
        match mm_app::App::check_directory_conflict(
            &state.app.config().import_directory,
            &state.app.config().plugin_directory,
        ) {
            Ok(false) => {}
            Ok(true) => {
                return Err(ApiError::from(*AppError::boxed(
                    "createUpload",
                    "api.upload.create.directory_conflict.app_error",
                    None,
                    String::new(),
                    403,
                ))
                .into());
            }
            Err(err) => {
                tracing::error!(error = %err, "the import/plugin directory check failed");
                return Err(ApiError::from(*AppError::boxed(
                    "createUpload",
                    "api.upload.create.check_directory.app_error",
                    None,
                    String::new(),
                    500,
                ))
                .into());
            }
        }
    } else {
        let (ok, _) = state
            .app
            .session_has_permission_to_channel(&session.0, &us.channel_id, &PERMISSION_UPLOAD_FILE)
            .await;
        if !ok {
            return Err(make_permission_error(&session.0, &[&PERMISSION_UPLOAD_FILE]).into());
        }
        us.type_ = UploadType(UploadType::ATTACHMENT.to_owned());
    }

    us.id = mm_model::utils::new_id();
    if !session.0.user_id.is_empty() {
        us.user_id.clone_from(&session.0.user_id);
    }

    if us.file_size > state.app.config().file_max_file_size {
        return Err(ApiError::from(*AppError::boxed(
            "createUpload",
            "api.upload.create.upload_too_large.app_error",
            Some(std::collections::HashMap::from([(
                "channelId".to_owned(),
                serde_json::Value::String(us.channel_id.clone()),
            )])),
            String::new(),
            413,
        ))
        .into());
    }

    let created = state.app.create_upload_session(us).await?;

    let mut out =
        serde_json::to_vec(&created).map_err(|err| marshal_error(&err, "createUpload"))?;
    out.push(b'\n');
    Ok(json_response(StatusCode::CREATED, out))
}

/// Port of `uploadData` (api4/upload.go:122), reached as
/// `POST /api/v4/uploads/{upload_id}`.
///
/// `EnableFileAttachments` off is 501; the id is validated; the session is read (404/500). The
/// import branch needs `manage_system` and refuses a cloud licence; the attachment branch needs
/// the caller to *own* the session **and** hold `upload_file` on its channel — a non-owner is a
/// 403 whether or not they have the channel permission. Then `doUploadData`: a multipart body's
/// first part, or the raw body with `Content-Length > FileSize - FileOffset` refused as the 400
/// `invalid_content_length`.
///
/// The answer is **204** for a chunk that does not complete the upload and the `FileInfo` JSON —
/// with a trailing newline — for the one that does.
#[tracing::instrument(skip_all, fields(upload_id = %upload_id, forwarded = false))]
pub async fn upload_data(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match serve_upload_data(&state, &upload_id, &session, &parts, body).await {
        Ok(response) => response,
        Err(Refusal::Api(err)) => err.into_response(),
        Err(Refusal::Forward(bytes)) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

async fn serve_upload_data(
    state: &AppState,
    upload_id: &str,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<Response, Refusal> {
    if !state.app.config().file_enable_file_attachments {
        return Err(attachments_disabled("uploadData", 501).into());
    }
    require_id(upload_id, "upload_id")?;

    let us = state.app.get_upload_session(upload_id).await?;

    if us.type_.as_str() == UploadType::IMPORT {
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
        {
            return Err(make_permission_error(&session.0, &[&PERMISSION_MANAGE_SYSTEM]).into());
        }
        if state
            .app
            .license()
            .await
            .ok()
            .flatten()
            .is_some_and(|license| license.is_cloud())
        {
            return Err(cloud_upload("UploadData").into());
        }
    } else if us.user_id != session.0.user_id {
        return Err(make_permission_error(&session.0, &[&PERMISSION_UPLOAD_FILE]).into());
    } else {
        let (ok, _) = state
            .app
            .session_has_permission_to_channel(&session.0, &us.channel_id, &PERMISSION_UPLOAD_FILE)
            .await;
        if !ok {
            return Err(make_permission_error(&session.0, &[&PERMISSION_UPLOAD_FILE]).into());
        }
    }

    // `doUploadData`: the request body, capped by the `MaxBytesReader` like every file-API route.
    let bytes = read_capped_body(state, parts, body).await?;
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());

    let chunk: Vec<u8> = match crate::multipart::boundary_of(content_type) {
        // `ErrNotMultipart`: a simple body. The declared-length check is Go's, before the read.
        Err(crate::multipart::MultipartError::NotMultipart) => {
            if let Some(length) = declared_content_length(&parts.headers) {
                if length > us.file_size - us.file_offset {
                    return Err(ApiError::from(*AppError::boxed(
                        "uploadData",
                        "api.upload.upload_data.invalid_content_length",
                        None,
                        String::new(),
                        400,
                    ))
                    .into());
                }
            }
            bytes.to_vec()
        }
        // A boundary, or `ErrMissingBoundary`: `doUploadData`'s multipart branch, and its
        // `invalid_content_type` 400 for any parse failure that is not `ErrNotMultipart`.
        Ok(_) | Err(_) => match crate::multipart::first_part(content_type, &bytes) {
            Ok(Some(part)) => part,
            Ok(None) | Err(_) => {
                return Err(ApiError::from(*AppError::boxed(
                    "uploadData",
                    "api.upload.upload_data.invalid_content_type",
                    None,
                    String::new(),
                    400,
                ))
                .into());
            }
        },
    };

    match state.app.upload_data(us, &chunk).await {
        Ok(None) => Ok(no_content()),
        Ok(Some(info)) => {
            let mut out =
                serde_json::to_vec(&info).map_err(|err| marshal_error(&err, "uploadData"))?;
            out.push(b'\n');
            Ok(json_response(StatusCode::OK, out))
        }
        Err(PrepareError::App(err)) => Err(Refusal::from(err)),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding the upload data to Go");
            Err(Refusal::Forward(bytes))
        }
    }
}

/// The `MaxBytesReader(w, r.Body, MaxFileSize + bytes.MinRead)` every `handlerParamFileAPI` route
/// wraps its body in (web/handlers.go:220). Past the cap Go's read fails; here that is an over-long
/// body, which for these two routes becomes the same parse/decode 400 the caller already gives —
/// so a body past the cap is truncated to it and left to fail naturally, which is the only
/// observable Go produces without a wrapped `MaxBytesError`.
async fn read_capped_body(
    state: &AppState,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<axum::body::Bytes, Refusal> {
    let cap = usize::try_from(state.app.config().file_max_file_size + BYTES_MIN_READ)
        .unwrap_or(usize::MAX);
    // One past the cap, so a body of exactly the cap is not mistaken for an over-long one.
    axum::body::to_bytes(body, cap.saturating_add(1))
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the upload body");
            let _ = parts;
            Refusal::Api(ApiError::invalid_param("upload"))
        })
}

fn attachments_disabled(where_: &'static str, status: i32) -> ApiError {
    ApiError::from(*AppError::boxed(
        where_,
        "api.file.attachments.disabled.app_error",
        None,
        String::new(),
        status,
    ))
}

fn cloud_upload(where_: &'static str) -> ApiError {
    ApiError::from(*AppError::boxed(
        where_,
        "api.file.cloud_upload.app_error",
        None,
        String::new(),
        400,
    ))
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

fn no_content() -> Response {
    (
        StatusCode::NO_CONTENT,
        [("x-mmrs-served-by", "rust")],
        axum::body::Body::empty(),
    )
        .into_response()
}

fn marshal_error(err: &serde_json::Error, where_: &'static str) -> Refusal {
    tracing::error!(error = %err, "failed to serialise the upload response");
    Refusal::Api(ApiError::from(*AppError::boxed(
        where_,
        "api.marshal_error",
        None,
        String::new(),
        500,
    )))
}
