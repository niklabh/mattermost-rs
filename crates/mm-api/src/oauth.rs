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

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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
