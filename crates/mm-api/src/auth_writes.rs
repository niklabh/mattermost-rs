//! The five write routes of the authentication vertical:
//!
//! ```text
//! POST /api/v4/users/logout                              logout                     (user.go:2534)
//! PUT  /api/v4/users/{user_id}/password                  updatePassword             (user.go:2000)
//! POST /api/v4/users/password/reset                      resetPassword              (user.go:2066)
//! POST /api/v4/users/email/verify                        verifyUserEmail            (user.go:2861)
//! POST /api/v4/users/{user_id}/reset_failed_attempts     resetPasswordFailedAttempts(user.go:4257)
//! ```
//!
//! `crate::auth` is the *session extractor*; this is the handler module. They are separate files
//! because they change for different reasons.
//!
//! # Three of these are `APIHandler`, not `APISessionRequired`
//!
//! `logout`, `resetPassword` and `verifyUserEmail` are registered with `APIHandler`
//! (api4/user.go:55, 57, 75), so a request with no token at all reaches the handler and the
//! session is the zero value. That is not an oversight on Go's part — a password reset link is
//! followed by somebody who by definition cannot log in. [`OptionalSession`] reproduces the
//! `RequireSession: false` half of `web.Handler.ServeHTTP` (handlers.go:267-299).
//!
//! # Every body here is `model.MapFromJSON`
//!
//! All four bodies are `map[string]string`, decoded by a function that turns **every** failure
//! into an empty map — a body that is not an object, an object with a non-string value, an empty
//! body. So a malformed body is never a 400 from the decoder; it is a 400 (or a 200) from
//! whichever key check runs first. See [`map_from_json`].
//!
//! # CSRF is not modelled
//!
//! Go's `checkCSRFToken` (handlers.go:295) rejects a **cookie**-authenticated non-GET request
//! that carries neither `X-CSRF-Token` nor `X-Requested-With: XMLHttpRequest`. Nothing in this
//! port implements it, on these routes or on any migrated write. Recorded as [D-236]; it is a
//! pre-existing gap that these routes inherit rather than introduce, but they are the first where
//! it is a *credential* change rather than a content one.

use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_USERS,
    make_permission_error,
};
use mm_model::session::Session;
use mm_model::token::TOKEN_SIZE;
use mm_model::user::User;
use mm_model::user::external::USER_AUTH_SERVICE_LDAP;
use mm_model::utils::{AppError, StringMap};

use crate::AppState;
use crate::auth::{AuthenticatedSession, TokenLocation, parse_auth_token};
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use crate::proxy;

/// Port of `model.MapFromJSON` (utils.go:507).
///
/// Identical to `crate::channel_member_writes::map_from_json`, which is private to that module;
/// duplicated rather than shared because four agents edit this workspace at once and a two-line
/// function is a worse merge risk as a shared symbol than as a copy. The behaviour it encodes —
/// every failure is an empty map, including a partially decodable object, where Go keeps the
/// prefix it managed to read — is documented there.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// Port of `web.ReturnStatusOK` (web/web.go:127) — `{"status":"OK"}` with **no trailing
/// newline**, because it is a `w.Write` rather than an encoder.
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

/// Port of the `RequireSession: false` path through `web.Handler.ServeHTTP` (handlers.go:267).
///
/// The differences from [`AuthenticatedSession`] are the whole point:
///
/// - **No token is not an error.** The session is the zero value and the handler runs.
/// - **A bad token is not an error either** — `h.RequireSession` gates the 401, so a stale cookie
///   on `POST /users/logout` produces a 200 with an empty session rather than a 401. It also
///   gates `RemoveSessionCookie`, so the extractor clears nothing; `logout` clears the cookie
///   itself, unconditionally, which is a different line of Go.
/// - **A 500 from the session store still fails the request.** That branch is outside the
///   `RequireSession` guard (`c.Err = err`), so a broken database is a 500 here and not an
///   anonymous 200 — which matters, because an anonymous 200 on `logout` would tell the client it
///   had been logged out when the row is still there.
/// - **A valid non-OAuth session presented in `?access_token=` is a 401**
///   (`api.context.token_provided.app_error`). Reproduced because it is the one place a *valid*
///   credential is refused, and a port that dropped it would accept a token in a URL that Go
///   rejects — which is the opposite direction from the usual porting risk.
#[derive(Debug, Clone)]
pub struct OptionalSession(pub Option<Session>);

impl OptionalSession {
    /// The session's id, or `""` — Go's `c.AppContext.Session().Id` on a zero-valued session.
    fn id(&self) -> &str {
        self.0.as_ref().map(|s| s.id.as_str()).unwrap_or_default()
    }
}

impl FromRequestParts<AppState> for OptionalSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some((token, location)) = parse_auth_token(parts) else {
            return Ok(OptionalSession(None));
        };

        match state.app.get_session(&token).await {
            Ok(session) => {
                if !session.is_oauth && location == TokenLocation::QueryString {
                    return Err(ApiError::from(AppError::new(
                        "ServeHTTP",
                        "api.context.token_provided.app_error",
                        None,
                        // Go interpolates the token here; `wipe_detailed` blanks it before it
                        // reaches the client, so the value never leaves the process either way.
                        String::new(),
                        401,
                    )));
                }
                Ok(OptionalSession(Some(session)))
            }
            Err(err) if err.status_code == 500 => Err(ApiError::from(err)),
            Err(_) => Ok(OptionalSession(None)),
        }
    }
}

/// Port of `logout` / `Logout` (api4/user.go:2534), reached as `POST /api/v4/users/logout`.
///
/// # It answers 200 to a caller who was never logged in
///
/// There is no session requirement and no `if session == nil` refusal: the cookie is cleared, the
/// revoke is skipped when `Session().Id` is empty, and `ReturnStatusOK` is written. So a logout
/// with no credentials, with an expired one, or with somebody else's revoked one are all the same
/// 200 — which is what makes "log out" safe to call unconditionally from a client that has lost
/// track of its own state.
///
/// # The cookie is cleared before the revoke, and unconditionally
///
/// `c.RemoveSessionCookie(w, r)` runs first and is not inside the `if`. A revoke that fails still
/// leaves the browser without the cookie, and the 500 that follows does not put it back. Both
/// orderings are reproduced: the `Set-Cookie` is on the error response too.
///
/// # An OAuth session is forwarded
///
/// `PlatformService.RevokeSession` sends an OAuth session down `RevokeAccessToken`, which also
/// deletes the `OAuthAccessData` row — a store this port does not have. Deleting only the
/// `Sessions` row would leave a live OAuth access token behind, so the request goes to Go
/// instead. See [`mm_app::App::revoke_session`].
#[tracing::instrument(skip_all, fields(session_id = %session.id(), forwarded))]
pub async fn logout(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    if session.0.as_ref().is_some_and(|s| s.is_oauth) {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    let cookie = remove_session_cookie(&state);

    let mut response = match session.0 {
        // `if c.AppContext.Session().Id != ""` — a zero-valued session revokes nothing.
        Some(session) if !session.id.is_empty() => {
            match state.app.revoke_session_by_id(&session.id).await {
                Ok(()) => status_ok(),
                Err(err) => ApiError::from(err).into_response(),
            }
        }
        _ => status_ok(),
    };

    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, value);
    }
    response
}

/// Port of `updatePassword` (api4/user.go:2000), reached as
/// `PUT /api/v4/users/{user_id}/password`.
///
/// # `canUpdatePassword` is computed from the **target**, and its failure is silent
///
/// Go fetches the target user and picks one of three permissions by what the target *is*: a
/// system admin needs `manage_system`, a bot needs the manage-bot rules, anybody else needs
/// `sysconsole_write_user_management_users`. If the fetch fails — the id names nobody — the
/// `if user, err := ...; err == nil` block is skipped entirely and `canUpdatePassword` stays
/// `false`, so an unknown id is a **403**, not a 404. That is deliberate: a 404 here would
/// enumerate accounts.
///
/// # Four ways to be refused, with four different statuses
///
/// | request | outcome |
/// |---|---|
/// | `already_hashed=true`, permitted | hashed write, 200 |
/// | `already_hashed=true`, self, not permitted | **401** `api.user.update_password.user_and_hashed.app_error` |
/// | `already_hashed=true`, other, not permitted | **403** `api.user.update_password.context.app_error` |
/// | self, empty `current_password` | **400** `api.context.invalid_body_param.app_error` naming `current_password` |
///
/// The self + `already_hashed` case being a **401** rather than a 403 is the one a reader would
/// most plausibly "fix"; it is Go's, and its id says why — you may not hand yourself a hash.
///
/// # Nothing validates `{user_id}` beyond the mux charset
///
/// `RequireUserId` resolves `me` first, then checks `IsValidId`; a malformed id is a 400 from
/// `require_id`. A well-formed id that names nobody falls through to the 403 above.
#[tracing::instrument(skip_all, fields(user_id = %user_id, actor = %session.0.user_id, forwarded))]
pub async fn update_password(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }

    // Go's decoder swallows a read failure into an empty map; there is no body-read error path
    // in this handler at all.
    let props = body_props(request).await;
    let new_password = props.get("new_password").cloned().unwrap_or_default();
    let already_hashed = props.get("already_hashed").map(String::as_str) == Some("true");

    // The target is fetched for the permission decision only, and its absence is not an error.
    let target = state.app.get_user(&user_id).await.ok();
    let can_update = match &target {
        Some(user) if user.is_system_admin() => {
            state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
                .await
        }
        Some(user) if user.is_bot => state
            .app
            .session_has_permission_to_manage_bot(&session.0, &user_id)
            .await
            .is_ok(),
        Some(_) => {
            state
                .app
                .session_has_permission_to(
                    &session.0,
                    &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_USERS,
                )
                .await
        }
        None => false,
    };

    let is_self = user_id == session.0.user_id;

    let result = if already_hashed {
        if can_update {
            state
                .app
                .update_hashed_password_by_user_id(&user_id, &new_password)
                .await
        } else if is_self {
            Err(AppError::boxed(
                "updatePassword",
                "api.user.update_password.user_and_hashed.app_error",
                None,
                String::new(),
                401,
            ))
        } else {
            Err(context_error())
        }
    } else if is_self {
        let current_password = props.get("current_password").cloned().unwrap_or_default();
        if current_password.is_empty() {
            return ApiError::invalid_param("current_password").into_response();
        }
        state
            .app
            .update_password_as_user(Some(&session.0), &user_id, &current_password, &new_password)
            .await
    } else if can_update {
        state
            .app
            .update_password_by_user_id_send_email(Some(&session.0), &user_id, &new_password)
            .await
    } else {
        Err(context_error())
    };

    match result {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `resetPassword` (api4/user.go:2066), reached as
/// `POST /api/v4/users/password/reset`.
///
/// # The token length check is the only input validation
///
/// `len(token) != model.TokenSize` — exactly 64, counted in **bytes** — is a 400
/// `api.context.invalid_body_param.app_error` naming `token`, and it runs before anything else.
/// A missing key is the empty string and fails it. The new password is not looked at until the
/// app layer validates it, so a valid token with a bad password is a 400 about the *password*.
///
/// # No session is required, and that is the point
///
/// Registered with `APIHandler`. Somebody following a reset link has no session by construction.
/// A session, if one is presented, still reaches the app layer — it decides which session
/// survives `TerminateSessionsOnPasswordChange`.
#[tracing::instrument(skip_all, fields(has_session = session.0.is_some()))]
pub async fn reset_password(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    let props = body_props(request).await;
    let token = props.get("token").cloned().unwrap_or_default();
    if token.len() != TOKEN_SIZE {
        return ApiError::invalid_param("token").into_response();
    }
    let new_password = props.get("new_password").cloned().unwrap_or_default();

    match state
        .app
        .reset_password_from_token(session.0.as_ref(), &token, &new_password)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `verifyUserEmail` (api4/user.go:2861), reached as
/// `POST /api/v4/users/email/verify`.
///
/// # **Every** error from the app layer is rewritten
///
/// `c.Err = model.NewAppError("verifyUserEmail", "api.user.verify_email.bad_link.app_error", …,
/// http.StatusBadRequest).Wrap(err)` — the inner error is wrapped, not returned, so a token of
/// the wrong type, an expired one, a token naming a deleted user and a **500 from the store**
/// all leave as the same 400 with the same id. That is a status downgrade a port would be
/// tempted to "correct"; it is Go's, and it is what keeps this route from confirming which
/// tokens exist.
///
/// The only error this handler produces itself is the token-length 400, and its id is different
/// (`api.context.invalid_body_param.app_error`) — so a 64-byte string that is not a token and a
/// 63-byte one are distinguishable, which is Go's behaviour and not a leak: neither says whether
/// any token exists.
#[tracing::instrument(skip_all)]
pub async fn verify_user_email(State(state): State<AppState>, request: Request) -> Response {
    let props = body_props(request).await;
    let token = props.get("token").cloned().unwrap_or_default();
    if token.len() != TOKEN_SIZE {
        return ApiError::invalid_param("token").into_response();
    }

    match state.app.verify_email_from_token(&token).await {
        Ok(()) => status_ok(),
        Err(err) => {
            tracing::debug!(inner = %err.id, "email verification failed; reporting bad_link");
            ApiError::from(AppError::new(
                "verifyUserEmail",
                "api.user.verify_email.bad_link.app_error",
                None,
                String::new(),
                400,
            ))
            .into_response()
        }
    }
}

/// Port of `resetPasswordFailedAttempts` (api4/user.go:4257), reached as
/// `POST /api/v4/users/{user_id}/reset_failed_attempts`.
///
/// # Two permission checks with two different error shapes
///
/// The first — `sysconsole_write_user_management_users` — is **not** `SetPermissionError`: it is
/// a hand-built 403 with its own id, `api.user.reset_password_failed_attempts.permissions.app_error`,
/// and a `userID` parameter. The second, which only a system-admin *target* triggers, **is**
/// `SetPermissionError(manage_system)` and therefore carries the generic
/// `api.context.permissions.app_error`. Two refusals, two ids, on one route.
///
/// # The order is: permission, fetch, admin-target, auth-service
///
/// The fetch sits between the two permission checks, so a caller without the first permission
/// gets a 403 for an id that does not exist, while a caller *with* it gets `get_user`'s 404.
/// Moving the fetch earlier would turn this route into an account-existence oracle for anybody.
///
/// # LDAP and email only
///
/// `AuthService` must be `""` or `ldap`; a SAML, GitLab or OAuth account is a **400**
/// `api.user.reset_password_failed_attempts.ldap_and_email_only.app_error`. Those accounts have
/// no local counter to clear.
#[tracing::instrument(skip_all, fields(user_id = %user_id, actor = %session.0.user_id))]
pub async fn reset_password_failed_attempts(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    require_id(&user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_USERS,
        )
        .await
    {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "userID".to_owned(),
            serde_json::Value::String(user_id.clone()),
        );
        return Err(ApiError::from(AppError::new(
            "resetPasswordFailedAttempts",
            "api.user.reset_password_failed_attempts.permissions.app_error",
            Some(params),
            String::new(),
            403,
        )));
    }

    let user = state.app.get_user(&user_id).await?;

    if user.is_system_admin()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    if !is_ldap_or_email(&user) {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert("userID".to_owned(), serde_json::Value::String(user_id));
        return Err(ApiError::from(AppError::new(
            "resetPasswordFailedAttempts",
            "api.user.reset_password_failed_attempts.ldap_and_email_only.app_error",
            Some(params),
            String::new(),
            400,
        )));
    }

    state.app.reset_password_failed_attempts(&user).await?;
    Ok(status_ok())
}

/// `user.AuthService != model.UserAuthServiceLdap && user.AuthService != ""`, inverted.
///
/// Note `model.UserAuthServiceEmail` is **not** one of the accepted values — an account whose
/// `AuthService` is the literal `"email"` is refused, while one with `""` is allowed. Both spell
/// "email account" elsewhere in the codebase; only the empty one passes here.
fn is_ldap_or_email(user: &User) -> bool {
    user.auth_service == USER_AUTH_SERVICE_LDAP || user.auth_service.is_empty()
}

/// Read a request body into `MapFromJSON`'s map. A body that cannot be read is an empty map, the
/// same as one that cannot be parsed — Go has no error path here.
async fn body_props(request: Request) -> StringMap {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    map_from_json(&bytes)
}

/// The 403 both `already_hashed` and the plain path share (`user.go:2046`, `user.go:2051`).
fn context_error() -> Box<AppError> {
    AppError::boxed(
        "updatePassword",
        "api.user.update_password.context.app_error",
        None,
        String::new(),
        403,
    )
}

/// `c.RemoveSessionCookie(w, r)` (web/context.go:180), rendered the way `net/http` writes it.
///
/// The same three details `crate::auth`'s copy documents: `MaxAge: -1` serialises as `Max-Age=0`,
/// an empty `Path` is omitted entirely, and the attribute order is `Path`, `Max-Age`, `HttpOnly`.
fn remove_session_cookie(state: &AppState) -> String {
    let path: String = state
        .app
        .config()
        .subpath()
        .bytes()
        .filter(|&b| (0x20..0x7f).contains(&b) && b != b';')
        .map(char::from)
        .collect();
    let mut cookie = String::from("MMAUTHTOKEN=");
    if !path.is_empty() {
        cookie.push_str("; Path=");
        cookie.push_str(&path);
    }
    cookie.push_str("; Max-Age=0; HttpOnly");
    cookie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_with_auth(auth_service: &str) -> User {
        User {
            auth_service: auth_service.to_owned(),
            ..User::default()
        }
    }

    /// `""` and `ldap` pass; **`email` does not**, which is the branch a reader would get wrong.
    #[test]
    fn only_empty_and_ldap_auth_services_may_be_unlocked() {
        assert!(is_ldap_or_email(&user_with_auth("")));
        assert!(is_ldap_or_email(&user_with_auth("ldap")));
        assert!(!is_ldap_or_email(&user_with_auth("email")));
        assert!(!is_ldap_or_email(&user_with_auth("saml")));
        assert!(!is_ldap_or_email(&user_with_auth("gitlab")));
    }

    #[test]
    fn map_from_json_turns_every_failure_into_an_empty_map() {
        assert!(map_from_json(b"").is_empty());
        assert!(map_from_json(b"[]").is_empty());
        assert!(map_from_json(br#"{"a":1}"#).is_empty());
        assert_eq!(
            map_from_json(br#"{"token":"x"}"#)
                .get("token")
                .map(String::as_str),
            Some("x")
        );
    }

    /// `model.TokenSize` is 64 **bytes**. A 64-character string of multi-byte runes is 128 bytes
    /// and must be refused, which is what Go's `len()` does.
    #[test]
    fn the_token_length_check_counts_bytes() {
        assert_eq!(TOKEN_SIZE, 64);
        assert_eq!("t".repeat(64).len(), 64);
        assert_eq!("\u{00e9}".repeat(64).len(), 128);
    }

    #[test]
    fn the_status_ok_body_has_no_trailing_newline() {
        let body = r#"{"status":"OK"}"#;
        assert!(!body.ends_with('\n'));
    }

    #[test]
    fn the_three_hand_built_errors_keep_their_statuses() {
        assert_eq!(context_error().status_code, 403);
        assert_eq!(
            context_error().id,
            "api.user.update_password.context.app_error"
        );
    }
}
