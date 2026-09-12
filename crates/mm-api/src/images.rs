//! The four routes that answer with a stored image and nothing else: the user's profile picture,
//! a team's icon, a custom emoji's image, and the installation's brand image.
//!
//! None of them uses `WriteFileResponse`. Each reads the whole file into memory and writes it
//! with two or three headers of its own, so there are no ranges, no `Last-Modified` and no
//! conditional requests except the two `HandleEtag` routes below. Four handlers, four different
//! sets of headers, and the differences are not systematic — which is why they are here together
//! rather than folded into `users.rs`, `teams.rs`, `emoji.rs` and a `brand.rs`.
//!
//! | route | `Cache-Control` | `ETag` | permission |
//! |---|---|---|---|
//! | `getProfileImage` | `max-age=86400, private` | the user's `LastPictureUpdate` | `UserCanSeeOtherUser` |
//! | `getTeamIcon` | `max-age=86400, private` | the team's `LastTeamIconUpdate` | `view_team`, or the team is open |
//! | `getEmojiImage` | `max-age=2592000, private` | none | none at all |
//! | `getBrandImage` | none | none | none at all |
//!
//! `getProfileImage` additionally drops its own `ETag` and shortens `Cache-Control` to five
//! minutes when it had to generate the image — a branch this port never takes, because it
//! forwards instead of generating.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_model::permission::{
    PERMISSION_EDIT_BRAND, PERMISSION_EDIT_OTHER_USERS, PERMISSION_VIEW_MEMBERS,
    PERMISSION_VIEW_TEAM, make_permission_error,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{require_id, resolve_me};
use crate::error::ApiError;
use crate::proxy;

/// `model.DayInSeconds` — what `getProfileImage` and `getTeamIcon` both format into their
/// `Cache-Control`.
const DAY_CACHE_CONTROL: &str = "max-age=86400, private";

/// `getEmojiImage`'s, which is thirty days rather than one and is a literal in the handler.
const EMOJI_CACHE_CONTROL: &str = "max-age=2592000, private";

/// `model.HeaderEtagClient` (model/client4.go) — the request header `HandleEtag` reads.
const ETAG_CLIENT_HEADER: &str = "If-None-Match";
/// `model.HeaderEtagServer`.
const ETAG_SERVER_HEADER: &str = "ETag";

/// Port of `web.Context.HandleEtag` (web/context.go:230).
///
/// **The server's `ETag` is written only inside the 304 branch.** A 200 from either etagged route
/// carries the header separately, because those two handlers set it themselves afterwards — but
/// `HandleEtag` itself does not, which is why this returns only the hit and leaves the 200's
/// header to the caller.
fn etag_hit(headers: &HeaderMap, etag: &str) -> bool {
    !etag.is_empty()
        && headers
            .get(ETAG_CLIENT_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|presented| presented == etag)
}

fn not_modified(etag: &str) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [(ETAG_SERVER_HEADER, etag), ("x-mmrs-served-by", "rust")],
    )
        .into_response()
}

/// An image body with its content type and cache header — the shape all four routes end in.
fn image_response(
    content_type: &str,
    cache_control: Option<&str>,
    etag: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let mut headers = vec![
        ("Content-Type".to_owned(), content_type.to_owned()),
        ("x-mmrs-served-by".to_owned(), "rust".to_owned()),
    ];
    if let Some(cache_control) = cache_control {
        headers.push(("Cache-Control".to_owned(), cache_control.to_owned()));
    }
    if let Some(etag) = etag {
        headers.push((ETAG_SERVER_HEADER.to_owned(), etag.to_owned()));
    }

    let mut response = (StatusCode::OK, body).into_response();
    for (name, value) in headers {
        match (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => {
                response.headers_mut().insert(name, value);
            }
            _ => tracing::error!(name, value, "refusing to set a malformed response header"),
        }
    }
    response
}

/// Port of `getProfileImage` (api4/user.go:290), reached as `GET /api/v4/users/{user_id}/image`.
///
/// # The etag is the user's `LastPictureUpdate`, rendered as a bare integer
///
/// `strconv.FormatInt(user.LastPictureUpdate, 10)` — no quotes, no `W/`, so it is not a
/// syntactically valid HTTP entity tag at all. `HandleEtag` compares it with `==` against
/// whatever the client sent, which is why that works; a client that quoted it would never get a
/// 304.
///
/// A user who has never uploaded a picture has `LastPictureUpdate == 0` and therefore the etag
/// `"0"`, which is a perfectly good cache key — it is not the empty string, so `HandleEtag`'s
/// `etag != ""` guard does not fire.
///
/// # The generated-avatar branch is forwarded
///
/// See [`mm_app::App::get_profile_image`]: when there is no stored image Go draws one from the
/// user's initials with a TTF rasteriser, and no reimplementation matches it pixel for pixel.
/// That branch also *writes* — it stores the generated image when `LastPictureUpdate == 0` — so
/// forwarding is doubly right.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn get_profile_image(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve_profile_image(&state, &user_id, &session, request.headers()).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn serve_profile_image(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> Result<Option<Response>, ApiError> {
    // `RequireUserId` substitutes the session's id for `me` **before** validating, so
    // `/users/me/image` is the caller's own picture and not a 400. See [`resolve_me`].
    let user_id = resolve_me(user_id, session);
    require_id(user_id, "user_id")?;

    // `UserCanSeeOtherUser` runs **before** the user is fetched, so a caller who may not see the
    // target gets a 403 whether or not the target exists.
    let can_see = match state
        .app
        .user_can_see_other_user(&session.0.user_id, user_id)
        .await
    {
        Ok(can_see) => can_see,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, user_id, "forwarding to Go");
            return Ok(None);
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };
    if !can_see {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_MEMBERS],
        )));
    }

    let user = state.app.get_user(user_id).await?;
    let etag = user.last_picture_update.to_string();
    if etag_hit(headers, &etag) {
        return Ok(Some(not_modified(&etag)));
    }

    match state.app.get_profile_image(user_id).await {
        Ok(image) => Ok(Some(image_response(
            "image/png",
            Some(DAY_CACHE_CONTROL),
            Some(&etag),
            image,
        ))),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, user_id, "forwarding to Go");
            Ok(None)
        }
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
    }
}

/// Port of `getTeamIcon` (api4/team.go:1023), reached as `GET /api/v4/teams/{team_id}/image`.
///
/// # The permission test is an `||` in disguise, and the team is fetched first so it can be
///
/// `!SessionHasPermissionToTeam(view_team) && (team.Type != open || !team.AllowOpenInvite)` — so
/// a caller with no membership at all may read the icon of an **open team that allows open
/// invites**. That is why `GetTeam` runs before the permission check rather than after it, and
/// why a non-existent team id is a 404 to everyone rather than a 403.
///
/// # `Content-Type: image/png` is asserted, not detected
///
/// Unlike `getEmojiImage`, this route does not decode the file. `SetTeamIcon` re-encodes every
/// upload as PNG, so the claim holds for anything the API wrote — and a file put there by other
/// means is served with the wrong type by both servers.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn get_team_icon(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve_team_icon(&state, &team_id, &session, request.headers()).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn serve_team_icon(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> Result<Option<Response>, ApiError> {
    require_id(team_id, "team_id")?;

    let team = state.app.get_team(team_id).await?;

    let has_permission = state
        .app
        .session_has_permission_to_team(&session.0, team_id, &PERMISSION_VIEW_TEAM)
        .await;
    let team_is_publicly_readable =
        team.team_type == mm_model::team::TEAM_OPEN && team.allow_open_invite;
    if !has_permission && !team_is_publicly_readable {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_TEAM],
        )));
    }

    let etag = team.last_team_icon_update.to_string();
    if etag_hit(headers, &etag) {
        return Ok(Some(not_modified(&etag)));
    }

    match state.app.get_team_icon(team_id).await {
        Ok(image) => Ok(Some(image_response(
            "image/png",
            Some(DAY_CACHE_CONTROL),
            Some(&etag),
            image,
        ))),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, team_id, "forwarding to Go");
            Ok(None)
        }
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
    }
}

/// Port of `getEmojiImage` (api4/emoji.go:222), reached as
/// `GET /api/v4/emoji/{emoji_id}/image`.
///
/// # It is `APIHandler`, not `APISessionRequired` — but the router still requires a session
///
/// Every route under `BaseRoutes.Emojis` is registered with `APISessionRequired`, this one
/// included (api4/emoji.go:20), so it is authenticated. What it does **not** have is any
/// permission check: any logged-in user may read any custom emoji's image.
///
/// # The content type comes from decoding the file, not from the row
///
/// `image.DecodeConfig` names the format and the handler concatenates `image/` onto it, so a GIF
/// emoji is `image/gif` — see [`mm_app::imaging`] for what that reproduces and what it does not.
#[tracing::instrument(skip_all, fields(emoji_id = %emoji_id))]
pub async fn get_emoji_image(
    State(state): State<AppState>,
    Path(emoji_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve_emoji_image(&state, &emoji_id).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

async fn serve_emoji_image(state: &AppState, emoji_id: &str) -> Result<Option<Response>, ApiError> {
    // `c.RequireEmojiId()`.
    require_id(emoji_id, "emoji_id")?;

    // The feature gate, and it is **501** here rather than the 403 the emoji *reads* use — one
    // setting, two statuses, one segment apart.
    if !state.app.config().enable_custom_emoji {
        return Err(ApiError::from(*mm_model::utils::AppError::boxed(
            "getEmojiImage",
            "api.emoji.disabled.app_error",
            None,
            String::new(),
            501,
        )));
    }

    match state.app.get_emoji_image(emoji_id).await {
        Ok((image, format)) => Ok(Some(image_response(
            &format!("image/{format}"),
            Some(EMOJI_CACHE_CONTROL),
            None,
            image,
        ))),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, emoji_id, "forwarding to Go");
            Ok(None)
        }
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
    }
}

/// Port of `getBrandImage` (api4/brand.go:20), reached as `GET /api/v4/brand/image`.
///
/// # Three things that are unlike every other route in this file
///
/// 1. **No permission check at all**, and it is `APIHandlerTrustRequester` rather than
///    `APISessionRequired` — so it answers an **unauthenticated** caller. The brand image is
///    shown on the login page, which is why.
/// 2. **Every failure is a bare 404 with an empty body.** The handler discards the `AppError`
///    entirely and calls `w.Write(nil)`, so there is no error id, no JSON, and no `Content-Type`
///    on the way out. A missing image, an unreadable one and a driverless configuration are one
///    answer.
/// 3. **No `Cache-Control`**, so the login page re-fetches the image every time.
#[tracing::instrument(skip_all)]
pub async fn get_brand_image(State(state): State<AppState>, request: Request) -> Response {
    match state.app.get_brand_image().await {
        Ok(image) => image_response("image/png", None, None, image),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            proxy::forward_to_go(State(state), request).await
        }
        // The discard is the port, not a shortcut: `getBrandImage` writes a 404 with a nil body
        // for *any* error, so distinguishing them here would be a divergence.
        //
        // `Content-Type: application/json` on a zero-byte body looks wrong and is exactly what
        // Go sends: `web.Handler.ServeHTTP` sets it on every API response *before* the handler
        // runs (web/handlers.go:260), and this branch never overrides it. Stated here because
        // nothing in `getBrandImage` mentions a content type at all.
        Err(PrepareError::App(_)) => (
            StatusCode::NOT_FOUND,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            Vec::<u8>::new(),
        )
            .into_response(),
    }
}

/// Port of `deleteBrandImage` (api4/brand.go:87), reached as `DELETE /api/v4/brand/image`.
///
/// The only write in this module, and the only route here with a permission check that is a plain
/// system permission: `edit_brand`. It is checked **after** the audit record is opened and before
/// anything touches the backend, so a caller without it never learns whether an image exists.
///
/// A missing image is a **404** — `api.admin.delete_brand_image.storage.not_found` — which is the
/// opposite of what `deleteExport` and `deleteImport` do with the same situation. Three deletes
/// over the same file backend, two opinions.
#[tracing::instrument(skip_all)]
pub async fn delete_brand_image(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_EDIT_BRAND)
        .await
    {
        return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_EDIT_BRAND]))
            .into_response();
    }

    match state.app.delete_brand_image().await {
        Ok(()) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            r#"{"status":"OK"}"#,
        )
            .into_response(),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(PrepareError::App(err)) => ApiError::from(*err).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// the three image writes and the generated default
// -------------------------------------------------------------------------------------------

/// `model.NewAppError("uploadProfileImage", …)` — every refusal in `setProfileImage` names
/// `uploadProfileImage` as its `where`, including the ones raised by `setDefaultProfileImage`.
fn profile_image_error(where_: &str, id: &str, status: i32) -> ApiError {
    ApiError::from(*mm_model::utils::AppError::boxed(
        where_,
        id,
        None,
        String::new(),
        status,
    ))
}

/// `*FileSettings.DriverName == ""` → **501**, shared verbatim by both halves of the profile
/// image write.
///
/// The `where` differs between them and the id does not: `setDefaultProfileImage` raises
/// `api.user.upload_profile_user.storage.app_error` under its own name (api4/user.go:706) while
/// `setProfileImage` raises the same id under `uploadProfileImage` (api4/user.go:619).
fn storage_not_configured(state: &AppState, where_: &'static str) -> Option<ApiError> {
    state.app.config().file_driver_name.is_empty().then(|| {
        profile_image_error(
            where_,
            "api.user.upload_profile_user.storage.app_error",
            501,
        )
    })
}

/// `r.ContentLength`, as an `i64`, or `None` where Go's is `-1`.
///
/// Go compares `r.ContentLength > MaxFileSize`, and an absent or unparseable `Content-Length`
/// leaves it at `-1`, which never exceeds anything. So a chunked upload of any size passes this
/// check and is caught — if at all — by the `MaxBytesReader` further in.
fn declared_content_length(headers: &HeaderMap) -> Option<i64> {
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
}

/// What `setProfileImage` and `uploadBrandImage` both do with a body before anything else looks
/// at it: refuse an over-long declared length, read it, refuse an over-long actual one, and parse
/// it as `multipart/form-data`.
///
/// # Two size limits that are 512 bytes apart
///
/// `r.ContentLength > *FileSettings.MaxFileSize` is the handler's own check. The body itself is
/// wrapped by `web.Handler.ServeHTTP` in `http.MaxBytesReader(w, r.Body, MaxFileSize +
/// bytes.MinRead)` (web/handlers.go:220) — `bytes.MinRead` is 512 — so the *read* fails 512 bytes
/// later than the declared-length check refuses. A body in that 512-byte window, sent without a
/// `Content-Length`, reaches `ParseMultipartForm` and fails there.
///
/// And the two callers then answer that read failure differently, which is the reason this
/// returns the distinction rather than one error:
///
/// * `setProfileImage` wraps the parse error (`.Wrap(err)`, api4/user.go:628), so
///   `handleContextError` finds the `MaxBytesError` inside it through `errors.As` and **replaces**
///   the whole thing with `api.context.request_body_too_large.app_error` at 413.
/// * `uploadBrandImage` does **not** wrap (api4/brand.go:50), so the `MaxBytesError` is invisible
///   to `errors.As` and the client gets the plain `api.admin.upload_brand_image.parse.app_error`
///   at 400.
///
/// One config value, two limits, and the same over-long body is a 413 on one route and a 400 on
/// the other.
enum BodyRefusal {
    /// `r.ContentLength` exceeded `MaxFileSize`; the caller's own `too_large` error id.
    DeclaredTooLarge,
    /// The `MaxBytesReader` cap was hit — `MaxFileSize + 512`. `setProfileImage` turns this into
    /// the global 413; `uploadBrandImage` into its own parse 400.
    ReadCapExceeded,
    /// Any other reason `ParseMultipartForm` failed, the caller's own parse error id.
    Unparseable,
}

/// `bytes.MinRead` (bytes/buffer.go:21) — the slack `web.Handler.ServeHTTP` adds to the cap so
/// that "file sizes close to max file size do not get cut off".
const BYTES_MIN_READ: i64 = 512;

async fn read_multipart_body(
    state: &AppState,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
) -> Result<(axum::body::Bytes, crate::multipart::Form), BodyRefusal> {
    let max_file_size = state.app.config().file_max_file_size;

    if declared_content_length(&parts.headers).is_some_and(|length| length > max_file_size) {
        return Err(BodyRefusal::DeclaredTooLarge);
    }

    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the upload body");
            BodyRefusal::Unparseable
        })?;
    if i64::try_from(bytes.len()).unwrap_or(i64::MAX) > max_file_size + BYTES_MIN_READ {
        return Err(BodyRefusal::ReadCapExceeded);
    }

    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    match crate::multipart::parse_form(content_type, &bytes) {
        Ok(form) => Ok((bytes, form)),
        Err(err) => {
            tracing::debug!(error = %err, "the upload body is not multipart/form-data");
            Err(BodyRefusal::Unparseable)
        }
    }
}

/// `handleContextError`'s rewrite of a `MaxBytesError` (web/handlers.go:406) — global, and
/// reached only by a handler that **wrapped** the read failure into its `AppError`.
fn request_body_too_large(where_: &str) -> ApiError {
    ApiError::from(*mm_model::utils::AppError::boxed(
        where_,
        "api.context.request_body_too_large.app_error",
        None,
        "Use the setting `MaximumPayloadSizeBytes` in Mattermost config to configure allowed \
         payload limit. Learn more about this setting in Mattermost docs at \
         https://docs.mattermost.com/configure/environment-configuration-settings.html#maximum-payload-size"
            .to_owned(),
        413,
    ))
}

/// Port of `setProfileImage` (api4/user.go:610), reached as
/// `POST /api/v4/users/{user_id}/image`.
///
/// # Ten refusals, and the last one is the only thing this route does not answer
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `user_id` is not an id | 400 `api.context.invalid_url_param.app_error` |
/// | 2 | not the caller, and no `edit_other_users` | 403 |
/// | 3 | `FileSettings.DriverName == ""` | **501** `api.user.upload_profile_user.storage.app_error` |
/// | 4 | `Content-Length` over `MaxFileSize` | 413 `api.user.upload_profile_user.too_large.app_error` |
/// | 5 | the body overran `MaxFileSize + 512` | 413 `api.context.request_body_too_large.app_error` |
/// | 6 | the multipart body does not parse | **500** `api.user.upload_profile_user.parse.app_error` |
/// | 7 | no `image` part | 400 `api.user.upload_profile_user.no_file.app_error` |
/// | 8 | the user does not exist | 400 `api.context.invalid_url_param.app_error` naming `user_id` |
/// | 9 | LDAP (or LDAP-synced SAML) owns the picture | 409 `…login_provider_attribute_set.app_error` |
/// | 10 | the profile-field lock applies | 409 `…profile_field_locked.app_error` |
///
/// Three of those are worth saying out loud:
///
/// * **6 is a 500, not a 400.** `createEmoji` answers 400 for the identical failure; this one
///   passes `http.StatusInternalServerError` (api4/user.go:628). A malformed body from a client
///   is a server error here, and that is Go's answer, not a transcription slip.
/// * **8 is `SetInvalidURLParam`, not the app's 404.** `GetUser` returns
///   `app.user.missing_account.const` at 404 and the handler *discards it* for a 400 naming
///   `user_id` — so a well-formed id that names nobody is a 400 on this route and a 404 on the
///   DELETE beside it, from the same call.
/// * **The permission check precedes the storage check**, so an unprivileged caller on a
///   driverless server gets the 403 and never learns the server cannot store images.
///
/// # The write forwards, before anything is written
///
/// `SetProfileImage` decodes the upload, rotates it by its EXIF orientation, `FillCenter`s it to
/// 128×128 and re-encodes it as PNG — every accepted upload, PNG or not, is replaced by Go's
/// encoder's output. There is no write-through case as there is for `createEmoji`, so this route
/// serves its ten refusals and hands over the moment one of them has not fired. See [D-411].
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn set_profile_image(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let (parts, body) = request.into_parts();
    let mut forwarded_body = axum::body::Bytes::new();

    match refuse_profile_image(
        &state,
        &user_id,
        &session,
        &parts,
        body,
        &mut forwarded_body,
    )
    .await
    {
        Ok(Some(response)) => response,
        Ok(None) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(forwarded_body));
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

/// Everything `setProfileImage` answers, as `Err`; `Ok(None)` is the hand-over.
///
/// Returns `Ok(Some(_))` for nothing at all — the success path of this route is entirely Go's —
/// but keeps the shape the other handlers in this module use so the forward is one branch rather
/// than a sentinel error.
async fn refuse_profile_image(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
    forwarded_body: &mut axum::body::Bytes,
) -> Result<Option<Response>, ApiError> {
    const WHERE: &str = "uploadProfileImage";

    let user_id = resolve_me(user_id, session);
    require_id(user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    if let Some(err) = storage_not_configured(state, WHERE) {
        return Err(err);
    }

    let (bytes, form) = match read_multipart_body(state, parts, body).await {
        Ok(parsed) => parsed,
        Err(BodyRefusal::DeclaredTooLarge) => {
            return Err(profile_image_error(
                WHERE,
                "api.user.upload_profile_user.too_large.app_error",
                413,
            ));
        }
        // The wrap is what makes this the global 413 rather than the parse 500 below.
        Err(BodyRefusal::ReadCapExceeded) => return Err(request_body_too_large(WHERE)),
        Err(BodyRefusal::Unparseable) => {
            return Err(profile_image_error(
                WHERE,
                "api.user.upload_profile_user.parse.app_error",
                500,
            ));
        }
    };
    // The body was consumed to parse it; keep the bytes for the hand-over.
    *forwarded_body = bytes;

    // `m.File["image"]` — and `len(imageArray) <= 0` right after it, which Go's parser cannot
    // produce: a key exists in `Form.File` only because a part was appended under it.
    if form.first_file("image").is_none() {
        return Err(profile_image_error(
            WHERE,
            "api.user.upload_profile_user.no_file.app_error",
            400,
        ));
    }

    let user = match state.app.get_user(user_id).await {
        Ok(user) => user,
        // `c.SetInvalidURLParam("user_id")` — the 404 from `GetUser` is thrown away.
        Err(_) => return Err(ApiError::invalid_url_param("user_id")),
    };

    // `user.IsLDAPUser() || (user.IsSAMLUser() && *SamlSettings.EnableSyncWithLdap)` — and then
    // `*LdapSettings.PictureAttribute != ""` over the whole thing, so a plain LDAP user on a
    // server that names no picture attribute is *not* refused.
    let config = state.app.config();
    let picture_comes_from_ldap =
        user.is_ldap_user() || (user.is_saml_user() && config.saml_enable_sync_with_ldap);
    if picture_comes_from_ldap && !config.ldap_picture_attribute.is_empty() {
        return Err(profile_image_error(
            WHERE,
            "api.user.upload_profile_user.login_provider_attribute_set.app_error",
            409,
        ));
    }

    if profile_image_locked(state, session, &user).await? == Some(true) {
        return Err(profile_image_error(
            WHERE,
            "api.user.upload_profile_user.profile_field_locked.app_error",
            409,
        ));
    }

    Ok(None)
}

/// `IsProfileImageLockedForUser`, with its one unanswerable case turned into the forward that
/// both callers want.
///
/// A licensed server whose other three conjuncts all hold is handed over rather than guessed at;
/// see [`mm_app::App::is_profile_image_locked_for_user`]. The forward happens before any write on
/// both routes, because both check the lock last.
async fn profile_image_locked(
    state: &AppState,
    session: &AuthenticatedSession,
    user: &mm_model::user::User,
) -> Result<Option<bool>, ApiError> {
    match state
        .app
        .is_profile_image_locked_for_user(&session.0, user)
        .await
    {
        Ok(locked) => Ok(Some(locked)),
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
        // `None` is the hand-over: a licensed server whose other three conjuncts hold.
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            Ok(None)
        }
    }
}

/// Port of `setDefaultProfileImage` (api4/user.go:694), reached as
/// `DELETE /api/v4/users/{user_id}/image`.
///
/// # It reads as a delete and it is a write
///
/// Nothing is removed. `SetDefaultProfileImage` **generates** the initials avatar and writes it
/// over `users/<id>/profile.png` (app/user.go:1026), zeroes `LastPictureUpdate` through
/// `ResetLastPictureUpdate`, and publishes a `user_updated` websocket event carrying the
/// sanitized user. So the route that looks like the one reproducible member of this family is the
/// one that depends most completely on the TTF rasteriser — see [D-204] — and it forwards.
///
/// # Four refusals, and one of them differs from the POST beside it
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `user_id` is not an id | 400 `api.context.invalid_url_param.app_error` |
/// | 2 | not the caller, and no `edit_other_users` | 403 |
/// | 3 | `FileSettings.DriverName == ""` | 501, `where` = `setDefaultProfileImage` |
/// | 4 | the profile-field lock applies | 409 `…profile_field_locked.app_error` |
///
/// Between 3 and 4 sits `GetUser`, and **its error is propagated** here where the POST discards
/// it: a well-formed id naming nobody is `app.user.missing_account.const` at **404** on this
/// route and a 400 naming `user_id` on the POST. Same call, same failure, two answers.
///
/// There is no `login_provider_attribute_set` check on this route either — an LDAP user whose
/// picture attribute is set may not *upload* a picture but may reset one.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn set_default_profile_image(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);

    match refuse_default_profile_image(&state, &user_id, &session).await {
        Ok(()) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

async fn refuse_default_profile_image(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
) -> Result<(), ApiError> {
    const WHERE: &str = "setDefaultProfileImage";

    let user_id = resolve_me(user_id, session);
    require_id(user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    if let Some(err) = storage_not_configured(state, WHERE) {
        return Err(err);
    }

    // Propagated, unlike `setProfileImage`'s.
    let user = state.app.get_user(user_id).await?;

    if profile_image_locked(state, session, &user).await? == Some(true) {
        return Err(profile_image_error(
            WHERE,
            "api.user.upload_profile_user.profile_field_locked.app_error",
            409,
        ));
    }

    Ok(())
}

/// Port of `getDefaultProfileImage` (api4/user.go:527), reached as
/// `GET /api/v4/users/{user_id}/image/default`.
///
/// # Three refusals, then the rasteriser
///
/// `RequireUserId`, then `UserCanSeeOtherUser` (403 `view_members`, raised *before* the user is
/// fetched so a caller who may not see the target learns nothing about whether it exists), then
/// `GetUser`'s own 404. Past that the answer is `createProfileImage`
/// (app/users/profile_picture.go:105): an FNV-1a hash of the user id picks one of 26 colours, the
/// uppercased first character of the username is rasterised at 64pt through `golang/freetype`
/// over `fonts/nunito-bold.ttf`, and the 128×128 RGBA is encoded with
/// `png.Encoder{CompressionLevel: BestCompression}`.
///
/// Every pixel of that depends on freetype's hinting and anti-aliasing and every byte of the
/// result on Go's PNG encoder, so it is forwarded — [D-204], unchanged. **Bots** take a different
/// branch (`botDefaultImage`, a `//go:embed` of a fixed PNG in the Go tree) which *is* a constant
/// and would be reproducible, but only by copying a binary out of the read-only reference tree;
/// it is forwarded with the rest rather than vendored.
///
/// # The literal segment under a parameter
///
/// This path is `/users/{user_id}/image/default`, one segment deeper than
/// `/users/{user_id}/image`, which this server already answers for GET. Registering it does not
/// shadow that route — see the router test — but the two are one typo apart.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn get_default_profile_image(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);

    match refuse_default_image_read(&state, &user_id, &session).await {
        Ok(()) => {
            tracing::Span::current().record("forwarded", true);
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

async fn refuse_default_image_read(
    state: &AppState,
    user_id: &str,
    session: &AuthenticatedSession,
) -> Result<(), ApiError> {
    let user_id = resolve_me(user_id, session);
    require_id(user_id, "user_id")?;

    let can_see = match state
        .app
        .user_can_see_other_user(&session.0.user_id, user_id)
        .await
    {
        Ok(can_see) => can_see,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, user_id, "forwarding to Go");
            return Ok(());
        }
        Err(PrepareError::App(err)) => return Err(ApiError::from(*err)),
    };
    if !can_see {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_MEMBERS],
        )));
    }

    state.app.get_user(user_id).await?;
    Ok(())
}

/// Port of `uploadBrandImage` (api4/brand.go:36), reached as `POST /api/v4/brand/image`.
///
/// # The permission check is fourth, not first
///
/// Unlike every other write in this module, `edit_brand` is tested **after** the body has been
/// read, parsed and found to contain an `image` part (api4/brand.go:69). So an unauthenticated —
/// well, unprivileged; the route is still session-required — caller sending a malformed body gets
/// the 400, not the 403, and learns the shape of the request before being refused it. Four
/// refusals precede the permission:
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `Content-Length` over `MaxFileSize` | 413 `api.admin.upload_brand_image.too_large.app_error` |
/// | 2 | the multipart body does not parse, *or* overran the read cap | 400 `api.admin.upload_brand_image.parse.app_error` |
/// | 3 | no `image` part | 400 `api.admin.upload_brand_image.no_file.app_error` |
/// | 4 | no `edit_brand` | 403 |
/// | 5 | `FileSettings.DriverName == ""` | 501 `api.admin.upload_brand_image.storage.app_error` |
///
/// **2 is one answer where `setProfileImage` gives two.** That handler wraps its parse error, so
/// `handleContextError` promotes a `MaxBytesError` inside it to the global 413; this one does not
/// wrap (api4/brand.go:50), so an over-long body is indistinguishable from a malformed one.
///
/// 5 lives in `SaveBrandImage` rather than the handler, which is why it comes after the
/// permission — see [`mm_app::App::save_brand_image`]. Everything past it re-encodes the image
/// and forwards ([D-411]); the forward is before the archive `MoveFile` and before the write.
///
/// # Success is 201, and it still has a body
///
/// `w.WriteHeader(http.StatusCreated)` followed by `ReturnStatusOK(w)` — so the response is a
/// **201** carrying `{"status":"OK"}`, not the 200 every other `ReturnStatusOK` route gives.
/// Never produced here, since the write forwards; recorded because it is the one thing about this
/// route a reader would get wrong.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn upload_brand_image(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let (parts, body) = request.into_parts();
    let mut forwarded_body = axum::body::Bytes::new();

    match refuse_brand_image(&state, &session, &parts, body, &mut forwarded_body).await {
        Ok(()) => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(forwarded_body));
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

async fn refuse_brand_image(
    state: &AppState,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    body: axum::body::Body,
    forwarded_body: &mut axum::body::Bytes,
) -> Result<(), ApiError> {
    const WHERE: &str = "uploadBrandImage";

    let parse_error =
        || profile_image_error(WHERE, "api.admin.upload_brand_image.parse.app_error", 400);

    let (bytes, form) = match read_multipart_body(state, parts, body).await {
        Ok(parsed) => parsed,
        Err(BodyRefusal::DeclaredTooLarge) => {
            return Err(profile_image_error(
                WHERE,
                "api.admin.upload_brand_image.too_large.app_error",
                413,
            ));
        }
        // No `.Wrap(err)` on this handler's parse error, so `handleContextError` cannot see the
        // `MaxBytesError` and the 413 rewrite never happens. Same body, different route, 400.
        Err(BodyRefusal::ReadCapExceeded | BodyRefusal::Unparseable) => return Err(parse_error()),
    };
    *forwarded_body = bytes;

    if form.first_file("image").is_none() {
        return Err(profile_image_error(
            WHERE,
            "api.admin.upload_brand_image.no_file.app_error",
            400,
        ));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_EDIT_BRAND)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_BRAND],
        )));
    }

    match state.app.save_brand_image().await {
        Ok(()) => Ok(()),
        Err(PrepareError::App(err)) => Err(ApiError::from(*err)),
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            Ok(())
        }
    }
}
