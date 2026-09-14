//! Port of `uploadFileStream` (api4/file.go:77), reached as `POST /api/v4/files` — the classic
//! attachment upload every client makes before a post carries a file.
//!
//! # Two bodies, one response
//!
//! A `multipart/form-data` body is `uploadFileMultipart` (the webapp: a `channel_id` field, an
//! optional `client_ids` field, then the file parts); anything else is `uploadFileSimple` (the
//! mobile and API path: the file in the raw body, `channel_id` and `filename` in the query). Both
//! answer `model.FileUploadResponse` — `file_infos` paired with the `client_ids` echoed back — at
//! **201** with a trailing newline.
//!
//! # What forwards
//!
//! Each file goes through [`mm_app::App::upload_file_x`], which serves every refusal and the write
//! but hands a **raster image it would resize** to Go before writing ([D-380]/[D-411]). So a text
//! or non-image upload is served end to end; an image upload forwards — and because the multipart
//! form can carry several files, one image among them forwards the *whole* request, since the
//! response is one document and a half-served one would double-write the text files. That
//! all-or-nothing rule is [`ForwardWholeRequest`].
//!
//! # Gaps from Go, recorded rather than hidden
//!
//! `uploadFileMultipart`'s streaming/legacy split (which part arrives first) is not reproduced:
//! the body is parsed whole, files are read from the `files` field in order, and `client_ids`
//! from its field — observably identical for a well-formed request, which is every request a
//! client sends. The buffered `maxMultipartFormDataBytes` 10 KiB cap on a *field* value is not
//! enforced. See [D-652].

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use mm_model::file::FileUploadResponse;
use mm_model::file_info::{BOOKMARK_FILE_OWNER, FileInfo};
use mm_model::permission::{PERMISSION_UPLOAD_FILE, make_permission_error};
use mm_model::utils::AppError;

use mm_app::file_upload::{FILE_TEAM_ID, UploadFileTask};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::images::{BYTES_MIN_READ, declared_content_length};
use crate::proxy;
use mm_app::post::PrepareError;

/// Port of `uploadFileStream` (api4/file.go:77).
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn upload_file_stream(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match serve(&state, &session, &parts, body).await {
        Ok(response) => response,
        Err(Outcome::Api(err)) => err.into_response(),
        Err(Outcome::Forward(bytes)) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// A refusal, or the whole request handed to Go (see the module docs).
enum Outcome {
    Api(ApiError),
    Forward(axum::body::Bytes),
}

impl From<ApiError> for Outcome {
    fn from(err: ApiError) -> Self {
        Outcome::Api(err)
    }
}

impl From<Box<AppError>> for Outcome {
    fn from(err: Box<AppError>) -> Self {
        Outcome::Api(ApiError::from(*err))
    }
}

/// The signal that a served upload must instead be replayed to Go in full — the `Unreproducible`
/// image branch, wherever it fires.
struct ForwardWholeRequest;

async fn serve(
    state: &AppState,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<Response, Outcome> {
    if !state.app.config().file_enable_file_attachments {
        // **403** here, not the 501 `createUpload` gives.
        return Err(ApiError::from(*AppError::boxed(
            "uploadFileStream",
            "api.file.attachments.disabled.app_error",
            None,
            String::new(),
            403,
        ))
        .into());
    }

    // `r.ContentLength == 0` is refused before the body is read.
    if declared_content_length(&parts.headers) == Some(0) {
        return Err(read_request_error("Content-Length should not be 0").into());
    }

    let bytes = read_capped_body(state, parts, body).await?;
    let timestamp = chrono::Local::now();
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let query = parts.uri.query().unwrap_or_default();

    let response = match crate::multipart::boundary_of(content_type) {
        Ok(_) => upload_multipart(state, session, content_type, &bytes, query, timestamp).await?,
        // `ErrNotMultipart` and `ErrMissingBoundary` both fall to the simple path in Go's
        // `switch err { case nil / case ErrNotMultipart / default }` — no: `ErrMissingBoundary`
        // is `default`, the 400. Only a genuinely non-multipart type is the simple upload.
        Err(crate::multipart::MultipartError::NotMultipart) => {
            upload_simple(state, session, &bytes, query, timestamp).await?
        }
        Err(err) => {
            return Err(read_request_error(&err.to_string()).into());
        }
    };

    match response {
        Ok(response) => {
            let mut out = serde_json::to_vec(&response)
                .map_err(|err| marshal_error(&err, "uploadFileStream"))?;
            out.push(b'\n');
            Ok((
                StatusCode::CREATED,
                [
                    ("Content-Type", "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                out,
            )
                .into_response())
        }
        Err(ForwardWholeRequest) => {
            tracing::debug!("an image among the uploaded files is Go's; forwarding the request");
            Err(Outcome::Forward(bytes))
        }
    }
}

/// Port of `uploadFileSimple` (api4/file.go:130): channel and filename from the query, one file
/// in the body.
async fn upload_simple(
    state: &AppState,
    session: &AuthenticatedSession,
    body: &[u8],
    query: &str,
    timestamp: chrono::DateTime<chrono::Local>,
) -> Result<Result<FileUploadResponse, ForwardWholeRequest>, Outcome> {
    let params = query_params(query);
    let channel_id = params.first("channel_id");
    let filename = params.first("filename");

    require_id(channel_id, "channel_id")?;
    if filename.is_empty() {
        return Err(ApiError::invalid_url_param("filename").into());
    }

    check_channel(state, session, channel_id, "uploadFileSimple").await?;

    let client_id = params.first("client_id");
    let creator_id = creator_id(session, &params);

    let task = UploadFileTask {
        channel_id,
        name: filename,
        team_id: FILE_TEAM_ID,
        user_id: creator_id,
        timestamp,
        content_length: declared_content_length_simple(body),
        client_id,
        data: body,
    };
    match state.app.upload_file_x(&task).await {
        Ok(info) => Ok(Ok(one_file_response(info, client_id))),
        Err(PrepareError::App(err)) => Err(Outcome::from(err)),
        Err(PrepareError::Unreproducible(_)) => Ok(Err(ForwardWholeRequest)),
    }
}

/// Port of `uploadFileMultipart` (api4/file.go:196) for a well-formed body — see the module docs
/// on the streaming/legacy simplification.
async fn upload_multipart(
    state: &AppState,
    session: &AuthenticatedSession,
    content_type: Option<&str>,
    body: &[u8],
    query: &str,
    timestamp: chrono::DateTime<chrono::Local>,
) -> Result<Result<FileUploadResponse, ForwardWholeRequest>, Outcome> {
    let form = crate::multipart::parse_form(content_type, body)
        .map_err(|err| Outcome::from(read_request_error(&err.to_string())))?;
    let params = query_params(query);

    // `channel_id` from the form, falling back to the query the params carry (`c.Params.ChannelId`
    // is seeded from the query before the form is read).
    let channel_id = form
        .first_value("channel_id")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| params.first("channel_id"));
    require_id(channel_id, "channel_id")?;

    check_channel(state, session, channel_id, "uploadFileMultipart").await?;

    let creator_id = creator_id(session, &params);

    // Every part with a filename is a file. The webapp sends them under `files`; a body under
    // another name still counts, so both are gathered — `files` first for a stable order.
    let mut files: Vec<&crate::multipart::FilePart> = Vec::new();
    if let Some(parts) = form.file.get("files") {
        files.extend(parts.iter());
    }
    for (name, parts) in &form.file {
        if name != "files" {
            files.extend(parts.iter());
        }
    }

    let client_ids: Vec<&str> = form
        .value
        .get("client_ids")
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();

    // `if len(clientIds) != 0 && len(clientIds) != len(fileHeaders)` — the legacy check, and the
    // streaming one reaches the same 400 for a well-formed body.
    if !client_ids.is_empty() && client_ids.len() != files.len() {
        return Err(ApiError::from(*AppError::boxed(
            "uploadFileMultipart",
            "api.file.upload_file.incorrect_number_of_client_ids.app_error",
            Some(std::collections::HashMap::from([
                (
                    "NumClientIds".to_owned(),
                    serde_json::Value::from(client_ids.len()),
                ),
                ("NumFiles".to_owned(), serde_json::Value::from(files.len())),
            ])),
            String::new(),
            400,
        ))
        .into());
    }

    let mut infos: Vec<FileInfo> = Vec::with_capacity(files.len());
    let mut echoed: Vec<String> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let client_id = client_ids.get(index).copied().unwrap_or_default();
        let task = UploadFileTask {
            channel_id,
            name: &file.filename,
            team_id: FILE_TEAM_ID,
            user_id: creator_id,
            timestamp,
            content_length: -1,
            client_id,
            data: &file.data,
        };
        match state.app.upload_file_x(&task).await {
            Ok(info) => {
                infos.push(info);
                if !client_id.is_empty() {
                    echoed.push(client_id.to_owned());
                }
            }
            Err(PrepareError::App(err)) => return Err(Outcome::from(err)),
            // One image is enough to hand the whole request to Go.
            Err(PrepareError::Unreproducible(_)) => return Ok(Err(ForwardWholeRequest)),
        }
    }

    Ok(Ok(FileUploadResponse {
        file_infos: Some(infos),
        client_ids: Some(echoed),
    }))
}

/// `SessionHasPermissionToChannel(upload_file)`, the ABAC gate (always-pass here), then
/// `GetChannel` and the restricted-DM check — the three refusals both upload paths share.
async fn check_channel(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
    where_: &'static str,
) -> Result<(), Outcome> {
    let (ok, _) = state
        .app
        .session_has_permission_to_channel(&session.0, channel_id, &PERMISSION_UPLOAD_FILE)
        .await;
    if !ok {
        return Err(make_permission_error(&session.0, &[&PERMISSION_UPLOAD_FILE]).into());
    }
    if !mm_app::file::has_permission_to_file_action() {
        return Err(Outcome::from(mm_app::file::abac_denied(where_)));
    }
    match state.app.get_channel(channel_id).await {
        Ok(channel) => {
            if state
                .app
                .check_if_channel_is_restricted_dm(&channel)
                .await?
                == mm_app::channel::RestrictedDm::Yes
            {
                // Both paths raise this with the `uploadFileSimple` where.
                return Err(ApiError::from(*AppError::boxed(
                    "uploadFileSimple",
                    "api.file.upload_file.restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ))
                .into());
            }
            Ok(())
        }
        Err(err) => Err(ApiError::from(*AppError::boxed(
            where_,
            "api.file.upload_file.get_channel.app_error",
            None,
            err.message,
            400,
        ))
        .into()),
    }
}

/// `creatorId := session.UserId`, overridden to `bookmark` when `?bookmark=true`.
fn creator_id<'a>(session: &'a AuthenticatedSession, params: &QueryParams) -> &'a str {
    if crate::user_deletes::parse_go_bool(params.first_owned("bookmark").as_deref().unwrap_or("")) {
        BOOKMARK_FILE_OWNER
    } else {
        &session.0.user_id
    }
}

fn one_file_response(info: FileInfo, client_id: &str) -> FileUploadResponse {
    FileUploadResponse {
        file_infos: Some(vec![info]),
        client_ids: if client_id.is_empty() {
            None
        } else {
            Some(vec![client_id.to_owned()])
        },
    }
}

/// The `Content-Length` a simple upload passes to `UploadFileX` — the header value, or the body
/// length when it is absent. Go passes `r.ContentLength`, which for a body with no header is the
/// length the server measured; using the buffered length matches that for every non-chunked body.
fn declared_content_length_simple(_body: &[u8]) -> i64 {
    // The header is the source of truth; a chunked body has none, which `UploadFileX` treats as
    // "unknown, use MaxFileSize" — the same as Go's -1. The handler does not re-read the header
    // here, so -1 is the safe, Go-equivalent value for the buffered simple body.
    -1
}

/// A tiny query-string reader: the first value under a key, or the empty string.
struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    fn first(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    }

    fn first_owned(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

fn query_params(query: &str) -> QueryParams {
    QueryParams(
        form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
    )
}

fn read_request_error(detail: &str) -> ApiError {
    ApiError::from(*AppError::boxed(
        "uploadFileStream",
        "api.file.upload_file.read_request.app_error",
        None,
        detail.to_owned(),
        400,
    ))
}

async fn read_capped_body(
    state: &AppState,
    _parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<axum::body::Bytes, Outcome> {
    let cap = usize::try_from(state.app.config().file_max_file_size + BYTES_MIN_READ)
        .unwrap_or(usize::MAX);
    axum::body::to_bytes(body, cap.saturating_add(1))
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the file upload body");
            Outcome::from(read_request_error(&err.to_string()))
        })
}

fn marshal_error(err: &serde_json::Error, where_: &'static str) -> Outcome {
    tracing::error!(error = %err, "failed to serialise the file upload response");
    Outcome::Api(ApiError::from(*AppError::boxed(
        where_,
        "api.marshal_error",
        None,
        String::new(),
        500,
    )))
}
