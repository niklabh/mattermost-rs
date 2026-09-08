//! Port of `getOAuthApps`, `getOAuthApp`, `getOAuthAppInfo` and `getAuthorizedOAuthApps`
//! (channels/api4/oauth.go:145, :178, :205, :313) — the four OAuth **app** reads, reached as
//! `GET /api/v4/oauth/apps`, `.../{app_id}`, `.../{app_id}/info` and
//! `GET /api/v4/users/{user_id}/oauth/apps/authorized`.
//!
//! The System Console's *Integrations → OAuth 2.0 Applications* page. Every write, and the whole
//! authorization flow, is still forwarded.
//!
//! # `client_secret` is on the wire for two of the four
//!
//! `getOAuthAppInfo` sanitises in the **handler** (oauth.go:216) and `getAuthorizedOAuthApps` in
//! the **app layer** (app/oauth.go:642). The other two do not sanitise at all — the admin list
//! hands every app's secret to anyone with `manage_oauth`, and the single-app read hands it to the
//! creator or a system-wide admin. That is Go's, it is what the console relies on to show the
//! secret, and it is the one thing here a "safe-looking" tidy-up would break.
//!
//! Two of the four sanitise, two do not, and the two that do sanitise in **different layers**.
//! Each is ported where Go put it, because "where" is what a reader checks.

use axum::extract::{Path, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::oauth::{OAuthApp, OAuthAppRequest};
use mm_model::oauth_dcr::{
    ClientRegistrationRequest, DCR_ERROR_INVALID_CLIENT_METADATA, DCR_ERROR_UNSUPPORTED_OPERATION,
};
use mm_model::permission::{
    PERMISSION_MANAGE_OAUTH, PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page};
use crate::error::ApiError;

/// Port of `getOAuthApps` (oauth.go:145).
///
/// # The refusal is **not** a permission error
///
/// Go builds it by hand: `api.command.admin_only.app_error` at 403 (oauth.go:147), not
/// `SetPermissionError`. So this route's 403 carries a *command* error id, and a client matching
/// on `api.context.permissions.app_error` will not recognise it.
///
/// # One of the two branches below cannot be reached
///
/// After the gate, `manage_system_wide_oauth` selects every app and `manage_oauth` selects the
/// caller's own. The gate has already required `manage_oauth`, so the trailing `else` is dead in
/// Go; and on a stock server the only role granting either permission is `system_admin`, which
/// holds **both** — so `GetOAuthAppsByCreator` is unreachable over HTTP as well. It is ported
/// because the branch is real, and tested at the store
/// (`mm-store/tests/db_oauth_apps_by_creator.rs`).
///
/// `json.Marshal` + `w.Write` (oauth.go:167, :173) — **no trailing newline**, unlike the two
/// single reads beside it.
#[tracing::instrument(skip_all, fields(page, per_page, scope, count))]
pub async fn get_oauth_apps(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return Err(admin_only());
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());

    let apps = if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH)
        .await
    {
        tracing::Span::current().record("scope", "every app");
        state.app.get_oauth_apps(page, per_page).await?
    } else {
        tracing::Span::current().record("scope", "own apps");
        state
            .app
            .get_oauth_apps_by_creator(&session.0.user_id, page, per_page)
            .await?
    };
    tracing::Span::current().record("count", apps.len());

    Ok(json_ok(encode(&apps)?, false))
}

/// Port of `getOAuthApp` (oauth.go:178).
///
/// Two refusals, and they name **different permissions**: `manage_oauth` before the lookup, and
/// `manage_system_wide_oauth` after it, when the caller is not the app's creator. The second one
/// runs *after* the app is fetched, so a missing app is a 404 for a caller who would have been
/// refused — order is wire-visible and is reproduced.
///
/// **No `Sanitize()`**: this route returns `client_secret`.
///
/// `json.NewEncoder(w).Encode` — a trailing newline, unlike the list.
#[tracing::instrument(skip_all, fields(app_id = %app_id, creator))]
pub async fn get_oauth_app(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !is_valid_id(&app_id) {
        return Err(ApiError::invalid_url_param("app_id"));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OAUTH],
        )));
    }

    let app = state.app.get_oauth_app(&app_id).await?;
    tracing::Span::current().record("creator", &app.creator_id);

    if app.creator_id != session.0.user_id
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH],
        )));
    }

    Ok(json_ok(encode(&app)?, true))
}

/// Port of `getOAuthAppInfo` (oauth.go:205).
///
/// **No permission check at all** — any authenticated session may read any app's public
/// description, which is what an OAuth consent screen needs. The protection is `Sanitize()`
/// (oauth.go:216), which blanks `ClientSecret` and nothing else: `callback_urls`, `homepage` and
/// `is_trusted` all stay.
#[tracing::instrument(skip_all, fields(app_id = %app_id))]
pub async fn get_oauth_app_info(
    State(state): State<AppState>,
    Path(app_id): Path<String>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !is_valid_id(&app_id) {
        return Err(ApiError::invalid_url_param("app_id"));
    }

    let mut app = state.app.get_oauth_app(&app_id).await?;
    // `OAuthApp.Sanitize` (model/oauth.go:164) — one field, and only this route calls it.
    app.client_secret = String::new();

    Ok(json_ok(encode(&app)?, true))
}

/// Port of `getAuthorizedOAuthApps` (oauth.go:313) —
/// `GET /api/v4/users/{user_id}/oauth/apps/authorized`, the *Security → OAuth 2.0 Applications*
/// panel in a user's own settings.
///
/// # A user route, not an admin one
///
/// The gate is `SessionHasPermissionToUser` — yourself, or an admin — and its refusal names
/// **`edit_other_users`**, a write permission on a read, exactly as `getUserAudits` does. So
/// `manage_oauth` has nothing to do with this route: a plain user reads their own authorisations
/// and is refused someone else's.
///
/// The apps come back **sanitised** (see `mm_app::oauth`), and `json.Marshal` + `w.Write`
/// (oauth.go:330, :336) means **no trailing newline** — the same shape as the admin list.
#[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
pub async fn get_authorized_oauth_apps(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `me` first, then `RequireUserId` (web/context.go:301).
    let user_id = crate::channels::resolve_me(&user_id, &session);
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&mm_model::permission::PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());

    let apps = state
        .app
        .get_authorized_apps_for_user(user_id, page, per_page)
        .await?;
    tracing::Span::current().record("count", apps.len());

    Ok(json_ok(encode(&apps)?, false))
}

/// `model.NewAppError("getOAuthApps", "api.command.admin_only.app_error", nil, "", 403)`.
fn admin_only() -> ApiError {
    ApiError::from(AppError::new(
        "getOAuthApps",
        "api.command.admin_only.app_error",
        None,
        String::new(),
        403,
    ))
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the oauth apps");
        ApiError::from(AppError::new(
            "getOAuthApps",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// `newline` is the whole difference between the list route and the two single reads.
fn json_ok(mut body: Vec<u8>, newline: bool) -> Response {
    if newline {
        body.push(b'\n');
    }
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

/// Port of `createOAuthApp` (api4/oauth.go:29) — `POST /api/v4/oauth/apps`.
///
/// # The body is an `OAuthAppRequest`, not an `OAuthApp`
///
/// Six fields are lifted across and **everything else the client sent is discarded** — including
/// `id`, `creator_id`, `create_at` and `client_secret`. So this route cannot be used to plant an
/// app with a chosen id or secret, which is why the app layer's "already has an id" refusal is
/// unreachable through it.
///
/// # `is_public` is not stored anywhere
///
/// It only decides whether a secret is generated. A public client keeps an **empty** secret, and
/// that emptiness is what `IsPublicClient` later reads to refuse a regeneration — so the flag
/// survives as the absence of a value rather than as a column.
///
/// # `is_trusted` needs `manage_system`, and is silently cleared without it
///
/// Not a refusal: a caller holding `manage_oauth` but not `manage_system` gets a `201` for an app
/// that is not trusted, whatever they asked for.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn create_oauth_app(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("oauth_app").into_response();
        }
    };
    let app_request: OAuthAppRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(err) => {
            tracing::debug!(error = %err, "oauth_app body did not decode");
            return ApiError::invalid_param("oauth_app").into_response();
        }
    };

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OAUTH],
        ))
        .into_response();
    }

    let mut app = OAuthApp {
        name: app_request.name,
        description: app_request.description,
        icon_url: app_request.icon_url,
        callback_urls: app_request.callback_urls,
        homepage: app_request.homepage,
        is_trusted: app_request.is_trusted,
        ..OAuthApp::default()
    };

    if !state
        .app
        .session_has_permission_to(&session.0, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
        .await
    {
        app.is_trusted = false;
    }

    app.creator_id = session.0.user_id.clone();
    app.is_dynamically_registered = false;

    match state
        .app
        .create_oauth_app_internal(&app, !app_request.is_public)
        .await
    {
        Ok(saved) => encoded_json(StatusCode::CREATED, &saved),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateOAuthApp` (api4/oauth.go:83) — `PUT /api/v4/oauth/apps/{app_id}`.
///
/// The permission ladder runs **before the body is read**, which is the opposite of the incoming
/// webhook update: a caller without `manage_oauth` gets a 403 for a body that would not have
/// decoded.
///
/// `200`, not the `201` its create sibling answers.
#[tracing::instrument(skip_all, fields(app_id = %app_id))]
pub async fn update_oauth_app(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(app_id): Path<String>,
    request: Request,
) -> Response {
    if let Err(err) = require_app_id(&app_id) {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OAUTH],
        ))
        .into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("oauth_app").into_response();
        }
    };
    let mut updated: OAuthApp = match serde_json::from_slice(&bytes) {
        Ok(app) => app,
        Err(err) => {
            tracing::debug!(error = %err, "oauth_app body did not decode");
            return ApiError::invalid_param("oauth_app").into_response();
        }
    };

    if updated.id != app_id {
        return ApiError::invalid_param("app_id").into_response();
    }

    let old_app = match state.app.get_oauth_app(&app_id).await {
        Ok(app) => app,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Some(response) = refuse_unless_owner_or_system_wide(&state, &session, &old_app).await {
        return response;
    }

    // Silently preserved, not refused — the same shape as the create path's clearing.
    if !state
        .app
        .session_has_permission_to(&session.0, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
        .await
    {
        updated.is_trusted = old_app.is_trusted;
    }

    match state.app.update_oauth_app(&old_app, &updated).await {
        Ok(saved) => encoded_json(StatusCode::OK, &saved),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `deleteOAuthApp` (api4/oauth.go:142) — `DELETE /api/v4/oauth/apps/{app_id}`.
#[tracing::instrument(skip_all, fields(app_id = %app_id))]
pub async fn delete_oauth_app(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(app_id): Path<String>,
) -> Response {
    if let Err(err) = require_app_id(&app_id) {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OAUTH],
        ))
        .into_response();
    }

    let app = match state.app.get_oauth_app(&app_id).await {
        Ok(app) => app,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Some(response) = refuse_unless_owner_or_system_wide(&state, &session, &app).await {
        return response;
    }

    match state.app.delete_oauth_app(&app.id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `regenerateOAuthAppSecret` (api4/oauth.go:186) —
/// `POST /api/v4/oauth/apps/{app_id}/regen_secret`.
///
/// The one extra gate: a **public client** — an app whose stored secret is empty — is refused
/// with `api.oauth.regenerate_secret.public_client.app_error` at 400, *after* both permission
/// checks. Giving it a secret would silently convert it to a confidential client.
#[tracing::instrument(skip_all, fields(app_id = %app_id))]
pub async fn regenerate_oauth_app_secret(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(app_id): Path<String>,
) -> Response {
    if let Err(err) = require_app_id(&app_id) {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OAUTH)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OAUTH],
        ))
        .into_response();
    }

    let app = match state.app.get_oauth_app(&app_id).await {
        Ok(app) => app,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Some(response) = refuse_unless_owner_or_system_wide(&state, &session, &app).await {
        return response;
    }

    if app.is_public_client() {
        return ApiError::from(AppError::new(
            "regenerateOAuthAppSecret",
            "api.oauth.regenerate_secret.public_client.app_error",
            None,
            format!("app_id={}", app.id),
            400,
        ))
        .into_response();
    }

    match state.app.regenerate_oauth_app_secret(&app).await {
        Ok(saved) => encoded_json(StatusCode::OK, &saved),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `registerOAuthClient` (api4/oauth.go:342) — `POST /api/v4/oauth/apps/register`.
///
/// # Not an `AppError` route
///
/// Dynamic Client Registration is an RFC 7591 endpoint, so every failure is a **DCR error
/// envelope** — `{"error": …, "error_description": …}` at `400` — written with
/// `w.WriteHeader` + an encoder, never through `c.Err`. A port that reached for `ApiError` would
/// answer the right status with a body no DCR client can parse.
///
/// # No session, and two gates
///
/// Go's comment says it plainly: "Session and permission checks removed for DCR endpoint to allow
/// external client registration". Then `EnableOAuthServiceProvider` and
/// `EnableDynamicClientRegistration`, both answering `unsupported_operation`.
///
/// **The second defaults to `false`**, so on a stock server this route's whole reachable
/// behaviour is the two gates and the decode — which is what is served here. A deployment that
/// turns DCR on is handed to Go, because the registration itself has a validation surface this
/// port has not been through.
///
/// Go also rate-limits it to 2/sec with a burst of 1. There is no rate limiter here; the forward
/// is what carries that for an enabled deployment, and for a disabled one there is nothing to
/// limit. See [D-192].
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn register_oauth_client(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return dcr_error(
                DCR_ERROR_INVALID_CLIENT_METADATA,
                "Invalid JSON in request body",
            );
        }
    };

    // **The decode comes first**, before either gate — so a malformed body on a server with the
    // feature off answers `invalid_client_metadata`, not `unsupported_operation`.
    if serde_json::from_slice::<ClientRegistrationRequest>(&bytes).is_err() {
        return dcr_error(
            DCR_ERROR_INVALID_CLIENT_METADATA,
            "Invalid JSON in request body",
        );
    }

    if !state.app.config().enable_oauth_service_provider {
        return dcr_error(
            DCR_ERROR_UNSUPPORTED_OPERATION,
            "OAuth service provider is disabled",
        );
    }

    if !state.app.config().enable_dynamic_client_registration {
        return dcr_error(
            DCR_ERROR_UNSUPPORTED_OPERATION,
            "Dynamic client registration is disabled",
        );
    }

    // Both gates open: the registration itself, its validation and its rate limit belong to Go
    // until they are ported.
    tracing::Span::current().record("forwarded", true);
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    crate::proxy::forward_to_go(State(state), request).await
}

/// The ownership ladder the update, delete and regenerate paths share: not the creator **and**
/// without `manage_system_wide_oauth` is a 403 naming the latter.
async fn refuse_unless_owner_or_system_wide(
    state: &AppState,
    session: &AuthenticatedSession,
    app: &OAuthApp,
) -> Option<Response> {
    if session.0.user_id == app.creator_id {
        return None;
    }
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH)
        .await
    {
        return None;
    }
    Some(
        ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM_WIDE_OAUTH],
        ))
        .into_response(),
    )
}

/// `RequireAppId` (web/context.go) — `IsValidId`.
fn require_app_id(app_id: &str) -> Result<(), ApiError> {
    if !is_valid_id(app_id) {
        return Err(ApiError::invalid_url_param("app_id"));
    }
    Ok(())
}

/// The DCR failure shape: `w.WriteHeader(400)` then an encoder, so a trailing newline.
fn dcr_error(error_type: &str, description: &str) -> Response {
    let body = mm_model::oauth_dcr::new_dcr_error(error_type, description);
    match mm_model::utils::go_json_marshal(&body) {
        Ok(json) => (
            StatusCode::BAD_REQUEST,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            json + "\n",
        )
            .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "Error while writing response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `json.NewEncoder(w).Encode` — a trailing newline.
fn encoded_json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match mm_model::utils::go_json_marshal(value) {
        Ok(json) => (
            status,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            json + "\n",
        )
            .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "Error while writing response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::oauth::OAuthApp;

    fn app() -> OAuthApp {
        OAuthApp {
            id: "mmrsoauth00000000000000001".to_owned(),
            creator_id: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            create_at: 1_788_636_490_668,
            update_at: 1_788_636_490_669,
            client_secret: "mmrssecret".to_owned(),
            name: "mmrs app".to_owned(),
            description: "a description".to_owned(),
            icon_url: "http://example.invalid/i.png".to_owned(),
            callback_urls: Some(vec!["http://example.invalid/cb".to_owned()]),
            homepage: "http://example.invalid/".to_owned(),
            is_trusted: true,
            mattermost_app_id: String::new(),
            is_dynamically_registered: false,
        }
    }

    /// The list has no newline, the single reads do. Four handlers over two files now differ on
    /// this; it is asserted rather than remembered.
    #[test]
    fn only_the_single_reads_end_in_a_newline() {
        let listed = json_ok(encode(&vec![app()]).expect("encodes"), false);
        let single = json_ok(encode(&app()).expect("encodes"), true);
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(single.status(), StatusCode::OK);
    }

    /// `Sanitize` blanks the secret and **nothing else** — a port that cleared the callback urls
    /// with it would break the consent screen it exists for.
    #[test]
    fn sanitize_blanks_the_secret_and_leaves_everything_else() {
        let mut sanitised = app();
        sanitised.client_secret = String::new();

        assert_eq!(sanitised.client_secret, "");
        assert_eq!(sanitised.callback_urls, app().callback_urls);
        assert_eq!(sanitised.homepage, app().homepage);
        assert!(sanitised.is_trusted);
        assert_eq!(sanitised.name, app().name);
    }

    /// The list's refusal is a **command** error id, not the permission one.
    #[test]
    fn the_lists_refusal_is_not_a_permission_error() {
        let err = admin_only();
        assert_eq!(err.0.id, "api.command.admin_only.app_error");
        assert_eq!(err.0.status_code, 403);
        assert_ne!(err.0.id, "api.context.permissions.app_error");
    }
}
