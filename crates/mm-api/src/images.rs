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
    PERMISSION_EDIT_BRAND, PERMISSION_VIEW_MEMBERS, PERMISSION_VIEW_TEAM, make_permission_error,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
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
