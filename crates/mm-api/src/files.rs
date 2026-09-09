//! Port of `getFileInfo` (channels/api4/file.go:841), reached as
//! `GET /api/v4/files/{file_id}/info`.
//!
//! # The only `/files/` route that returns JSON
//!
//! Its five siblings under `BaseRoutes.File` — `""`, `/thumbnail`, `/preview`, `/link` and the
//! unauthenticated `/public` — all serve file *bytes* or a signed URL out of the file backend,
//! which this port does not have. They are unregistered and fall to `Router::fallback`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_model::file_info::{BOOKMARK_FILE_OWNER, FileInfo};
use mm_model::permission::{PERMISSION_READ_CHANNEL_CONTENT, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::posts::FILE_CACHE_CONTROL;
use crate::proxy;
use crate::serve_content::{FileResponse, FileResponseSpec, write_file_response};

/// Port of `getFileInfo` (api4/file.go:841).
///
/// # The permission block is three branches, not two, and the middle one is the surprise
///
/// ```text
/// if info.CreatorId == model.BookmarkFileOwner {
///     if !perm { 403 }
/// } else if info.CreatorId != session.UserId && !perm {
///     403
/// }
/// ```
///
/// A file **you uploaded** is readable without any channel permission at all — `CreatorId ==
/// session.UserId` short-circuits the second branch — but a *bookmark* file is not: the literal
/// owner `"bookmark"` can never equal a real user id, so the first branch exists solely to
/// remove that escape hatch. Collapsing the two into a single `!perm` test would deny a user
/// their own file after they left the channel; collapsing them the other way would hand every
/// user every channel bookmark.
///
/// `perm` comes from `SessionHasPermissionToReadChannel`, and the 403 reports
/// `read_channel_content` in both branches.
///
/// # The channel lookup runs before the permission check and can 404 on its own
///
/// `info.ChannelId` is `COALESCE(FileInfo.ChannelId, '')` in the store, and the column really is
/// nullable — it was added after `FileInfo` existed. The upload path fills it in
/// (`t.fileinfo.ChannelId = t.ChannelId`, app/file.go:774), so every row the current server
/// writes has one; a row from before that migration does not, arrives here as the empty string,
/// and `GetChannel("")` finds nothing. That is a **404** `app.channel.get.existing.app_error`
/// and not a 403 — so such a file is unreadable even by the user who uploaded it, because the
/// `CreatorId == session.UserId` escape hatch is two lines too late to matter.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(info)` — **trailing newline** ([D-086]) — plus
/// `Cache-Control: max-age=2592000, private`. There is no etag on this route at all, so
/// `If-None-Match` is ignored and every request is a 200.
///
/// # `First-Inaccessible-File-Time` is not reproduced
///
/// `setInaccessibleFileHeader` (api4/file.go:1032) sets it when `GetFileInfo` fails with
/// `app.file.cloud.get.app_error`, which needs a licence carrying a `Files` limit. Unreachable
/// here — the same treatment as `First-Inaccessible-Post-Time` in [`crate::posts`].
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_file_info(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve(&state, &file_id, &session).await {
        Ok(Some(response)) => response,
        // The mini-preview repair — see `mm_app::App::get_file_info`.
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn serve(
    state: &AppState,
    file_id: &str,
    session: &AuthenticatedSession,
) -> Result<Option<Response>, ApiError> {
    // `c.RequireFileId()` (web/context.go:455). The router's `[A-Za-z0-9]+` charset has already
    // turned away everything gorilla would 404, leaving the length check to this.
    require_id(file_id, "file_id")?;

    let info = match state.app.get_file_info(file_id).await {
        Ok(info) => info,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, file_id, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(err)),
    };

    let channel = state.app.get_channel(&info.channel_id).await?;

    let (perm, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;

    let denied = if info.creator_id == BOOKMARK_FILE_OWNER {
        !perm
    } else {
        info.creator_id != session.0.user_id && !perm
    };
    if denied {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    // Unconditionally true on this deployment; see `mm_app::file::has_permission_to_file_action`.
    if !mm_app::file::has_permission_to_file_action() {
        return Err(ApiError::from(*mm_app::file::abac_denied("getFileInfo")));
    }

    let mut body = serde_json::to_vec(&info).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise FileInfo");
        ApiError::from(AppError::new(
            "getFileInfo",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok(Some(
        (
            StatusCode::OK,
            [
                ("Cache-Control", FILE_CACHE_CONTROL),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    ))
}

/// `api4.ThumbnailImageType` and `api4.PreviewImageType` (api4/file.go:27) — **both**
/// `image/jpeg`, and both hard-coded rather than derived from the stored file.
const THUMBNAIL_IMAGE_TYPE: &str = "image/jpeg";
const PREVIEW_IMAGE_TYPE: &str = "image/jpeg";

/// `?download=` — `strconv.ParseBool` with the error discarded, so `?download=yes` is false.
const DOWNLOAD_PARAM: &str = "download";

/// Which of the three byte routes is being served.
///
/// They agree on the permission block, the ABAC gate, the plugin hook and the response writer,
/// and differ in exactly four places — which path they read, which content type they claim, what
/// `contentSize` they pass, and what they do when the path is empty. A table rather than three
/// near-identical handlers, because the *differences* are the whole content of this module and
/// three copies would bury them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteRoute {
    /// `getFile` (api4/file.go:519).
    Original,
    /// `getFileThumbnail` (api4/file.go:641).
    Thumbnail,
    /// `getFilePreview` (api4/file.go:772).
    Preview,
}

impl ByteRoute {
    /// The handler name Go puts in the `where` of its errors.
    fn where_(self) -> &'static str {
        match self {
            Self::Original => "getFile",
            Self::Thumbnail => "getFileThumbnail",
            Self::Preview => "getFilePreview",
        }
    }

    /// The `Content-Type` handed to `WriteFileResponse`. Only the original uses the stored
    /// `MimeType`; the two derived images are always `image/jpeg`, whatever the original was.
    fn content_type(self, info: &FileInfo) -> &str {
        match self {
            Self::Original => &info.mime_type,
            Self::Thumbnail => THUMBNAIL_IMAGE_TYPE,
            Self::Preview => PREVIEW_IMAGE_TYPE,
        }
    }

    /// The `contentSize` argument. **`0` for the derived images**, which outside `gzip` mode
    /// changes nothing — `ServeContent` overwrites `Content-Length` from the file's real size.
    fn content_size(self, info: &FileInfo) -> i64 {
        match self {
            Self::Original => info.size,
            Self::Thumbnail | Self::Preview => 0,
        }
    }

    /// The path in the file backend.
    fn backend_path(self, info: &FileInfo) -> &str {
        match self {
            Self::Original => &info.path,
            Self::Thumbnail => &info.thumbnail_path,
            Self::Preview => &info.preview_path,
        }
    }

    /// The 400 raised when a derived image does not exist.
    ///
    /// `FileInfo.Path` is never empty for a row the upload path wrote, so the original has no
    /// such branch; a thumbnail or preview path is empty for every non-image, and asking for one
    /// is `api.file.get_file_thumbnail.no_thumbnail.app_error` at **400** — not a 404, and the
    /// `detailed_error` carries `file_id=<id>`, which is the only place in this family where one
    /// is populated.
    fn missing_derived_image(self, file_id: &str) -> Option<Box<AppError>> {
        let id = match self {
            Self::Original => return None,
            Self::Thumbnail => "api.file.get_file_thumbnail.no_thumbnail.app_error",
            Self::Preview => "api.file.get_file_preview.no_preview.app_error",
        };
        Some(AppError::boxed(
            self.where_(),
            id,
            None,
            format!("file_id={file_id}"),
            400,
        ))
    }
}

/// Port of `getFile` (api4/file.go:519), reached as `GET`/`HEAD /api/v4/files/{file_id}`.
///
/// # It reads the row through `GetByIds`, not `GetFileInfo`, and that is three differences
///
/// `getFile` calls `Store().FileInfo().GetByIds([]{id}, true, true, false)` directly — bypassing
/// the app layer — so unlike its two siblings it:
///
/// 1. **Sees deleted rows** (`includeDeleted = true`), and then 404s them itself unless the
///    caller is a content reviewer. The error id it uses for that is deliberately the same one it
///    uses for a row that never existed, so a deleted file cannot be told apart from an absent
///    one.
/// 2. **Skips the mini-preview repair**, because that lives in `App.GetFileInfo`. So this route
///    never writes, and never has to be forwarded for the reason `getFileInfo` sometimes is.
/// 3. Carries a **different error id** for a missing row — `api.file.get_file_info.app_error`
///    rather than `app.file_info.get.app_error` — at the same 404.
///
/// # `as_content_reviewer` is forwarded
///
/// The flagged-content branch needs a licence carrying Enterprise Advanced, and the four checks
/// behind it (`requireContentFlaggingEnabled`, `checkChannelFlaggable`, `requireTeamContentReviewer`,
/// `requireFlaggedPost`) reach into a store this port does not have. A request carrying the
/// parameter is forwarded whole, so Go answers with whichever of the four refusals is really its
/// own. See [D-206].
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    serve_bytes(
        ByteRoute::Original,
        State(state),
        &file_id,
        &session,
        query,
        request,
    )
    .await
}

/// Port of `getFileThumbnail` (api4/file.go:641).
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_file_thumbnail(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    serve_bytes(
        ByteRoute::Thumbnail,
        State(state),
        &file_id,
        &session,
        query,
        request,
    )
    .await
}

/// Port of `getFilePreview` (api4/file.go:772).
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_file_preview(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    serve_bytes(
        ByteRoute::Preview,
        State(state),
        &file_id,
        &session,
        query,
        request,
    )
    .await
}

/// The body all three share.
///
/// # The order of the checks is the specification
///
/// Go runs them in this order and each one can answer, so moving any of them changes what a
/// caller sees: id shape → row → channel → (deleted) → permission → ABAC → *for the preview
/// only*, the empty-path 400 → plugin hook → backend read. The preview checks its empty path
/// **before** the plugin hook and the thumbnail checks it **after** — Go's comment says so
/// explicitly ("no point in running hook if there's no preview") — which is invisible here
/// because there is no plugin host, and reproduced in the ordering anyway so that adding one
/// later does not silently move it.
async fn serve_bytes(
    route: ByteRoute,
    State(state): State<AppState>,
    file_id: &str,
    session: &AuthenticatedSession,
    query: Option<String>,
    request: Request,
) -> Response {
    // The method and headers are taken by value rather than borrowed from `request`, because
    // `axum::body::Body` is `Send` but **not `Sync`** — so holding a `&Request` across an `await`
    // makes the handler's future non-`Send` and it stops being a `Handler` at all. The error
    // rustc gives for that is "the trait bound `Handler` is not satisfied", which says nothing
    // about the cause.
    let method = request.method().clone();
    let headers = request.headers().clone();

    match serve_bytes_inner(
        route,
        &state,
        file_id,
        session,
        query.as_deref(),
        &method,
        &headers,
    )
    .await
    {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_bytes_inner(
    route: ByteRoute,
    state: &AppState,
    file_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> Result<Option<Response>, ApiError> {
    require_id(file_id, "file_id")?;

    // `getFile` alone reads this parameter, and it takes a whole branch of licensed checks with
    // it. The two derived-image routes never look at it, so a request carrying it there is
    // ordinary and is served.
    if route == ByteRoute::Original && crate::channels::is_content_reviewer_request(query) {
        tracing::debug!("as_content_reviewer needs the content-flagging store; forwarding");
        return Ok(None);
    }

    let force_download = crate::channels::query_flag_is_true(query, DOWNLOAD_PARAM);

    let info = match route {
        ByteRoute::Original => match state.app.get_file_info_including_deleted(file_id).await {
            Ok(info) => info,
            Err(err) => return Err(ApiError::from(err)),
        },
        // The two derived routes go through `App.GetFileInfo`, which carries the mini-preview
        // repair — a *write* — and is forwarded when it would fire.
        ByteRoute::Thumbnail | ByteRoute::Preview => match state.app.get_file_info(file_id).await {
            Ok(info) => info,
            Err(PrepareError::Unreproducible(reason)) => {
                tracing::debug!(reason, file_id, "forwarding to Go");
                return Ok(None);
            }
            Err(PrepareError::App(err)) => return Err(ApiError::from(err)),
        },
    };

    let channel = state.app.get_channel(&info.channel_id).await?;

    // A deleted row is only reachable through `getFile`, and only a content reviewer may see it —
    // and a content-reviewer request has already been forwarded above. So this is unconditional
    // here, and it answers with the *same* id and status as a row that does not exist.
    if route == ByteRoute::Original && info.delete_at != 0 {
        return Err(ApiError::from(*AppError::boxed(
            "getFile",
            "api.file.get_file_info.app_error",
            None,
            String::new(),
            404,
        )));
    }

    let (perm, _is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, &channel)
        .await;

    let denied = if info.creator_id == BOOKMARK_FILE_OWNER {
        !perm
    } else {
        info.creator_id != session.0.user_id && !perm
    };
    if denied {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL_CONTENT],
        )));
    }

    if !mm_app::file::has_permission_to_file_action() {
        return Err(ApiError::from(*mm_app::file::abac_denied(route.where_())));
    }

    // `RunFileWillBeDownloadedHook` sits here. There is no plugin host, so `rejectionReason` is
    // always `""` and the 403 it would raise is unreachable — the same treatment every other
    // migrated route gives a plugin hook.

    let path = route.backend_path(&info);
    if path.is_empty() {
        if let Some(err) = route.missing_derived_image(&info.id) {
            return Err(ApiError::from(*err));
        }
    }

    let (file, size) = match state.app.file_reader(path).await {
        Ok(open) => open,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, file_id, "forwarding to Go");
            return Ok(None);
        }
        // `c.Err.StatusCode = http.StatusNotFound` — the handler *overwrites* the 500 `FileReader`
        // raised, keeping its id. So a backend that is down and a file that is gone are both
        // `api.file.file_reader.app_error` at 404.
        Err(PrepareError::App(err)) => {
            let mut err = *err;
            err.status_code = 404;
            return Err(ApiError::from(err));
        }
    };

    let spec = FileResponseSpec {
        filename: &info.name,
        content_type: route.content_type(&info),
        content_size: route.content_size(&info),
        // `time.Unix(0, info.UpdateAt*int64(1000*1000))` — epoch milliseconds, which is what
        // `UpdateAt` already is.
        modtime_millis: Some(info.update_at),
        webserver_mode: &state.app.config().webserver_mode,
        force_download,
    };

    match write_file_response(&spec, method, headers, file, size).await {
        FileResponse::Response(response) => Ok(Some(response)),
        FileResponse::Forward(reason) => {
            tracing::debug!(reason, file_id, "forwarding to Go");
            Ok(None)
        }
    }
}

/// `?h=` — the public link hash `getPublicFile` compares.
const PUBLIC_LINK_HASH_PARAM: &str = "h";

/// Port of `getPublicFile` (api4/file.go:889), reached as `GET`/`HEAD /files/{file_id}/public`.
///
/// # The only unauthenticated route in this family, and the only one whose errors are not JSON
///
/// Its path is outside `/api/`, so `web.Handler` renders a failure as a **signed HTML page** —
/// `utils.RenderWebAppError` with the server's `AsymmetricSigningKey` — rather than as an
/// `AppError` document. Reproducing even its 403 therefore means porting ECDSA signing and the
/// web-app error template ([D-170]).
///
/// So this handler serves **only the fully successful path** and forwards everything else: the
/// disabled setting, an unknown salt, a missing or wrong hash, a missing row, a missing file.
/// That is not a gap in coverage — the bytes are the whole point of the route, and every failure
/// is answered by the server that can render it.
///
/// # `EnablePublicLink` defaults to `false`, so a stock server forwards every request here
///
/// Which is the correct answer: on that server the route is a 403 page, and this port cannot draw
/// it. The success path exists for the deployment that has turned public links on.
#[tracing::instrument(skip_all, fields(file_id = %file_id))]
pub async fn get_public_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    request: Request,
) -> Response {
    let query = request.uri().query().map(str::to_owned);
    let method = request.method().clone();
    let headers = request.headers().clone();

    match serve_public_file(&state, &file_id, query.as_deref(), &method, &headers).await {
        Some(response) => response,
        None => proxy::forward_to_go(State(state), request).await,
    }
}

/// `None` means "Go would answer with something this port cannot draw" — see the handler's docs.
async fn serve_public_file(
    state: &AppState,
    file_id: &str,
    query: Option<&str>,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> Option<Response> {
    // `c.RequireFileId()` — a 400 rendered as HTML, so it forwards like every other failure.
    if require_id(file_id, "file_id").is_err() {
        return None;
    }

    let config = state.app.config();
    if !config.enable_public_link {
        tracing::debug!("EnablePublicLink is off; Go renders an HTML 403");
        return None;
    }
    if config.public_link_salt.is_empty() {
        // Go's default salt is `NewRandomString(32)`, generated at `SetDefaults` — so an empty
        // one here means the configuration document did not carry it and we would be validating
        // against a salt we invented. Every hash would fail and every request would 400.
        tracing::debug!("PublicLinkSalt is unknown to this server; forwarding");
        return None;
    }

    let info = state.app.get_file_info(file_id).await.ok()?;

    let hash = crate::channels::query_first(query, PUBLIC_LINK_HASH_PARAM)?;
    if !mm_app::file::public_link_hash_matches(&info.id, &config.public_link_salt, &hash) {
        tracing::debug!(
            file_id,
            "public link hash did not match; Go renders an HTML 400"
        );
        return None;
    }

    // `RunFileWillBeDownloadedHook` with an empty user id sits here; no plugin host, so it never
    // rejects.

    let (file, size) = state.app.file_reader(&info.path).await.ok()?;

    let spec = FileResponseSpec {
        filename: &info.name,
        content_type: &info.mime_type,
        content_size: info.size,
        modtime_millis: Some(info.update_at),
        webserver_mode: &config.webserver_mode,
        // Hard-coded `false` — this route has no `?download=`, unlike its three siblings.
        force_download: false,
    };

    match write_file_response(&spec, method, headers, file, size).await {
        FileResponse::Response(response) => Some(response),
        FileResponse::Forward(reason) => {
            tracing::debug!(reason, file_id, "forwarding to Go");
            None
        }
    }
}
