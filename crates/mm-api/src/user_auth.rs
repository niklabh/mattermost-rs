//! The authentication-data routes (api4/user.go):
//!
//! ```text
//! PUT  /api/v4/users/{user_id}/auth           updateUserAuth     (user.go:1859)
//! PUT  /api/v4/users/{user_id}/mfa            updateUserMfa      (user.go:1910)
//! POST /api/v4/users/{user_id}/mfa/generate   generateMfaSecret  (user.go:1969)
//! POST /api/v4/users/login/switch             switchAccountType  (user.go:2919)
//! ```
//!
//! These four decide **how an account authenticates**, so the thing to get right is not what each
//! one writes but what each one refuses, and — on `/auth` — what it silently ignores.
//!
//! # `updateUserAuth` trusts the body for exactly two fields
//!
//! The body decodes into `model.UserAuth`, which has `auth_data` and `auth_service` and nothing
//! else. A body carrying `roles`, `email`, `password` or `mfa_active` is accepted, those keys are
//! discarded by the decoder, and the response echoes only the two fields — measured against the
//! running server, which left `roles` at `system_user` and the address untouched for a body
//! asking for `system_admin` and a new e-mail. `the_body_cannot_smuggle_a_role_or_an_address` in
//! `parity/user_auth.rs` is that request.
//!
//! What it *does* do is larger than its name: the store blanks `Password`, zeroes
//! `FailedAttempts` and bumps `UpdateAt`, and the app layer then revokes every session. See
//! [`mm_app::user_auth`].
//!
//! # The gate order on `/auth` is unusual and observable
//!
//! `if !c.IsSystemAdmin()` runs **before** `RequireUserId`, which is the opposite of every other
//! route in this family. So a non-administrator naming a malformed id gets a **403**, while an
//! administrator naming the same id gets a 400 — measured, because reading the two checks in
//! source order is exactly how a port gets this backwards.
//!
//! # MFA: the gates serve, anything that would touch a secret forwards
//!
//! `ServiceSettings.EnableMultifactorAuthentication` is off on this deployment and cannot be
//! turned on through the API, and `Users.MfaActive` cannot be set through it either. Every
//! reachable answer on the MFA pair is therefore a refusal decided from a `SELECT` and the
//! configuration:
//!
//! | request | answer | served |
//! |---|---|---|
//! | malformed `{user_id}` | 400 `api.context.invalid_url_param.app_error` | yes |
//! | an OAuth-app session | 403 naming `edit_other_users` | yes |
//! | somebody else's account, no `edit_other_users` | 403 | yes |
//! | `{}` / a non-boolean `activate` | 400 naming `activate` | yes |
//! | `{"activate":true}` with no `code` | 400 naming `code` | yes |
//! | `{"activate":true,"code":…}`, unknown id | 404 `app.user.missing_account.const` | yes |
//! | `{"activate":true,…}`, `auth_service` not `""`/`ldap` | 400 `api.user.activate_mfa.email_and_ldap_only.app_error` | yes |
//! | `{"activate":true,…}`, otherwise | 501 `mfa.mfa_disabled.app_error` | yes |
//! | `{"activate":false}`, unknown id | 404 | yes |
//! | `{"activate":false}`, otherwise | 200 `{"status":"OK"}` | **forwarded** |
//! | `mfa/generate`, unknown id | 404 | yes |
//! | `mfa/generate`, otherwise | 501 | yes |
//! | either route, `EnableMultifactorAuthentication` on | — | **forwarded** |
//!
//! Two forwards, both taken before anything is written. The deactivation forward is the one that
//! costs a 200: `DeactivateMfa` has no configuration gate, writes `MfaActive = false` and
//! `MfaSecret = ''`, and then sends an MFA-change e-mail from a goroutine — a side effect after
//! the write, and there is no e-mail service here ([D-238]). The flag forward is the one that
//! matters: past it Go mints 160 bits of `crypto/rand` and renders a QR code, or validates a TOTP
//! token against `dgoogauth`. Neither is ported and neither could be tested against Go if it
//! were, since both sides would be comparing different random numbers. See [D-500].
//!
//! # `switchAccountType` is four routes wearing one path
//!
//! The body's `current_service`/`new_service` pair selects between four `App` functions and a
//! 400; `saml` counts as "OAuth" for the first two, and `ldap` gets its own pair. What each
//! branch can honestly answer differs, so the matrix is in [`switch_account_type`] rather than
//! here.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::license::LicenseState;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM, make_permission_error,
};
use mm_model::switch_request::SwitchRequest;
use mm_model::user::UserAuth;
use mm_model::user::external::USER_AUTH_SERVICE_LDAP;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::auth_writes::OptionalSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;
use crate::proxy;
use crate::user_creates::decode_go_struct;
use crate::user_updates::{split_body, status_ok, string_interface_from_json};

/// `json.NewEncoder(w).Encode(v)` — a 200 and a **trailing newline**, which is the encoder and not
/// the value ([D-086]).
fn json_response<T: serde::Serialize>(handler: &'static str, value: &T) -> Response {
    let mut body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, handler, "failed to serialise the response");
            return ApiError::from(AppError::new(
                handler,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');
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

/// `c.SetPermissionError(model.PermissionEditOtherUsers)` followed by
/// `c.Err.DetailedError += ", attempted access by oauth app"` — three handlers in this file spell
/// it identically.
///
/// The suffix never reaches a client (`WipeDetailed` blanks `detailed_error` unless
/// `EnableDeveloper` is on, and it is not), so it is reproduced for the log and for the day the
/// setting is ported, not for the wire.
fn oauth_app_refusal(session: &AuthenticatedSession) -> Response {
    let mut err = make_permission_error(&session.0, &[&PERMISSION_EDIT_OTHER_USERS]);
    err.detailed_error
        .push_str(", attempted access by oauth app");
    ApiError::from(err).into_response()
}

/// The prologue `updateUserMfa` and `generateMfaSecret` share, in Go's order: resolve `me`, check
/// the id, refuse an OAuth-app session, then `SessionHasPermissionToUser`.
///
/// **`RequireUserId` comes first here and last on `/auth`.** Measured: a non-administrator asking
/// `POST /users/notanid/mfa/generate` gets the 400, while the same caller asking
/// `PUT /users/notanid/auth` gets the 403.
///
/// `Response` is 128 bytes, which clippy calls a large `Err` — but it is the *refusal*, and
/// boxing it here would unbox it again at both call sites. Same exemption `decode_go_struct`
/// carries.
#[allow(clippy::result_large_err)]
async fn mfa_prologue(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: String,
) -> Result<String, Response> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return Err(err.into_response());
    }
    tracing::Span::current().record("user_id", &user_id);

    if session.0.is_oauth {
        return Err(oauth_app_refusal(session));
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response());
    }

    Ok(user_id)
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/auth
// ---------------------------------------------------------------------------------------------

/// Port of `updateUserAuth` (api4/user.go:1859).
///
/// # Nothing here forwards
///
/// Every branch is decided from the body, the session's roles and one `UPDATE`, and the tail —
/// `RevokeAllSessions` — is ported. The two steps Go takes that this does not are
/// `InvalidateCacheForUser`, which is an in-process cache this server has never had ([D-085]),
/// and the audit record ([D-028]).
///
/// # An id that matches no row is a 200
///
/// `UpdateAuthData` has no existence check and Go discards the row count, so
/// `PUT /users/aaaaaaaaaaaaaaaaaaaaaaaaaa/auth` with a valid body answers 200 with the submitted
/// `UserAuth` echoed back, having written nothing. Measured, not inferred, and asserted in
/// `parity/user_auth.rs` — a port that added the obvious `GetUser` would turn a 200 into a 404.
///
/// # `{"auth_service":"email"}` answers `{}`
///
/// `IsValid` requires `auth_data` to be *absent* for `email`, the handler then blanks
/// `auth_service`, and both fields carry `omitempty` — so the response body is two bytes. See
/// [`mm_app::user_auth::normalise_email_auth_service`].
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, auth_service))]
pub async fn update_user_auth(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `if !c.IsSystemAdmin()` — before `RequireUserId`, and the error names `edit_other_users`
    // rather than the `manage_system` it actually tested for.
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let (_request, bytes) = match split_body(request, "user").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    // `c.SetInvalidParamWithErr("user", jsonErr)` — the parameter is called `user` even though
    // the type is a `UserAuth` and the route is `/auth`.
    let user_auth: UserAuth = match decode_go_struct(&bytes, "user") {
        Ok(user_auth) => user_auth,
        Err(err) => return err.into_response(),
    };

    if !user_auth.is_valid() {
        return ApiError::from(AppError::new(
            "updateUserAuth",
            "api.user.update_user_auth.invalid_request",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    let user_auth = mm_app::user_auth::normalise_email_auth_service(&user_auth);
    tracing::Span::current().record("auth_service", &user_auth.auth_service);

    match state.app.update_user_auth(&user_id, &user_auth).await {
        Ok(updated) => json_response("updateUserAuth", &updated),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/mfa
// ---------------------------------------------------------------------------------------------

/// Port of `updateUserMfa` (api4/user.go:1910).
///
/// # The flag forward is taken where Go reads the flag, not where it would write
///
/// Go's next line after the permission check is `c.App.MFARequired(c.AppContext)`, guarded by
/// `!session.Local && session.UserId != c.Params.UserId`. `MFARequired` returns `nil` immediately
/// unless the server is licensed **and** `EnableMultifactorAuthentication` **and**
/// `EnforceMultifactorAuthentication` are all on, so with the flag off it cannot fire and the
/// whole line is a no-op. With the flag on it can, and it sits ahead of the two body-parameter
/// 400s — so the forward is taken here rather than after the parse, or an administrator with a
/// malformed body would get this server's 400 where Go gives the enforcement error.
///
/// # `activate` is `props["activate"].(bool)`, not a parse
///
/// A type assertion on a `map[string]any`, so `"true"`, `1` and `null` are all
/// `api.context.invalid_body_param.app_error` naming `activate` — only a JSON boolean passes. The
/// `code` check below it is the same assertion plus a non-empty test, and it runs **before**
/// `GetUser`: `{"activate":true}` for an id that does not exist is the 400, not the 404.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id, activate))]
pub async fn update_user_mfa(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = match mfa_prologue(&state, &session, user_id).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    if state.app.config().enable_multifactor_authentication {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    let (request, bytes) = match split_body(request, "activate").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let props = string_interface_from_json(&bytes);
    let Some(activate) = props.get("activate").and_then(serde_json::Value::as_bool) else {
        return ApiError::invalid_param("activate").into_response();
    };
    tracing::Span::current().record("activate", activate);

    let code = if activate {
        match props.get("code").and_then(serde_json::Value::as_str) {
            Some(code) if !code.is_empty() => code,
            // `!ok || code == ""` — a missing key, a non-string and an empty string are one answer.
            _ => return ApiError::invalid_param("code").into_response(),
        }
    } else {
        ""
    };

    if activate {
        return match state.app.activate_mfa(&user_id, code).await {
            // Unreachable while the flag is off, which the forward above guarantees; kept so the
            // arm is not a `panic!` if that ever changes.
            Ok(()) => status_ok(),
            Err(err) => ApiError::from(err).into_response(),
        };
    }

    // The deactivation arm. `DeactivateMfa`'s only read is `GetUser`, so its 404 is served here
    // and the two `UPDATE`s plus the MFA-change e-mail that follows them are Go's — decided
    // before anything is written, which is the whole constraint. See the module doc and [D-500].
    if let Err(err) = state.app.get_user(&user_id).await {
        return ApiError::from(err).into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/{user_id}/mfa/generate
// ---------------------------------------------------------------------------------------------

/// Port of `generateMfaSecret` (api4/user.go:1969).
///
/// Go sets `Cache-Control: no-cache`, `Pragma: no-cache` and `Expires: 0` immediately before
/// writing the secret. Those three lines are on the success path only, which this server never
/// reaches — with the flag on the whole request is forwarded, and with it off the answer is a 501
/// whose headers come from the error renderer. Named here so that a future port of the generator
/// does not lose them.
#[tracing::instrument(skip_all, fields(forwarded = false, user_id))]
pub async fn generate_mfa_secret(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = match mfa_prologue(&state, &session, user_id).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    if state.app.config().enable_multifactor_authentication {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    match state.app.generate_mfa_secret(&user_id).await {
        Ok(secret) => json_response("generateMfaSecret", &secret),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/login/switch
// ---------------------------------------------------------------------------------------------

/// The two configuration refusals `SwitchOAuthToEmail` and `SwitchLdapToEmail` share, in order.
///
/// Both are **403** and both name `api.user.auth_switch.not_available.*`. The second is one test
/// over two flags: e-mail *and* username sign-in must both be off for it to fire, so a server
/// that allows username logins alone still passes.
fn email_signin_refusals(state: &AppState, where_: &'static str) -> Option<Response> {
    let config = state.app.config();
    if !config.enable_sign_up_with_email {
        return Some(
            ApiError::from(AppError::new(
                where_,
                "api.user.auth_switch.not_available.email_signup_disabled.app_error",
                None,
                String::new(),
                403,
            ))
            .into_response(),
        );
    }
    if !config.enable_sign_in_with_email && !config.enable_sign_in_with_username {
        return Some(
            ApiError::from(AppError::new(
                where_,
                "api.user.auth_switch.not_available.login_disabled.app_error",
                None,
                String::new(),
                403,
            ))
            .into_response(),
        );
    }
    None
}

/// Port of `switchAccountType` (api4/user.go:2919).
///
/// # The branch table, and what each branch can honestly answer
///
/// `current_service` → `new_service` selects one of four `App` functions; anything else is a
/// **400** naming `switch_request`, which is also the answer for a body that will not decode.
/// `saml` is in the "OAuth" set for the first two rows and `ldap` is not; a `saml → ldap` request
/// matches nothing and is the 400.
///
/// | branch | served here | forwarded |
/// |---|---|---|
/// | no match, or a body that will not decode | the 400 | — |
/// | `email` → `saml`/`gitlab`/`google`/`office365`/`openid` | the unknown-address 404 | everything else |
/// | `email` → `ldap` | the unknown-address 404 | everything else |
/// | one of the five → `email` | every refusal, through `not_oauth_user` | the password change |
/// | `ldap` → `email` | **everything**, unlicensed | the whole branch, licensed |
///
/// # Why the first two forward so early
///
/// `SwitchEmailToOAuth` and `SwitchEmailToLdap` both call `CheckPasswordAndAllCriteria`, which
/// **writes**: it claims a `FailedAttempts` slot before it compares anything. A forward taken
/// after that would leave a consumed login attempt behind for a request Go then handled from
/// scratch, and three of them would lock an account out of a password it typed correctly once —
/// the same constraint `updateUser`'s e-mail change is under. The only gate ahead of it is
/// `GetUserByEmail`, so that 404 is all there is to serve.
///
/// # Why `ldap → email` serves the whole branch
///
/// `SwitchLdapToEmail` reaches `ldapInterface == nil || user.AuthData == nil` before it validates
/// anything or writes anything, and `ldapInterface` is nil in this build: `RegisterLdapInterface`
/// is called only from the enterprise import package, which is not in the pinned tree. So the
/// branch terminates at a **501** for every account that gets past the earlier refusals,
/// licence or no licence — measured against the running server with a real `ldap` account, not
/// inferred. The licensed case is forwarded anyway, because the gate above all four branches
/// (`License() != nil && !ExperimentalEnableAuthenticationTransfer`) reads a setting this port
/// does not carry; unlicensed, that gate is skipped entirely by its first conjunct. See [D-501].
///
/// # The success body, for the day one of these branches lands
///
/// `w.Write([]byte(model.MapToJSON(map[string]string{"follow_link": link})))` — a `Write`, so
/// **no trailing newline**, unlike the `json.NewEncoder` responses in the rest of this file. No
/// branch here produces it: `email → saml` mints a SAML relay token, `email → gitlab` and its
/// three siblings reach `GetAuthorizationCode`, and both of the `→ email` branches change a
/// password. All five are Go's.
///
/// # This handler takes no session, and one branch demands one
///
/// `switchAccountType` is an `APIHandler`: `RequireSession` is false, so a *bad* token is not a
/// 401 and does not clear the cookie — the session is simply empty. `c.SessionRequired()` inside
/// the `OAuthToEmail` branch is what turns an empty session into
/// `api.context.session_expired.app_error`, and only for that branch. [`OptionalSession`] is that
/// distinction.
#[tracing::instrument(skip_all, fields(forwarded = false, branch))]
pub async fn switch_account_type(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    let (request, bytes) = match split_body(request, "switch_request").await {
        Ok(pair) => pair,
        Err(err) => return err.into_response(),
    };
    let switch: SwitchRequest = match decode_go_struct(&bytes, "switch_request") {
        Ok(switch) => switch,
        Err(err) => return err.into_response(),
    };

    if switch.email_to_oauth() {
        tracing::Span::current().record("branch", "email_to_oauth");
        return switch_from_email(state, request, &switch.email).await;
    }
    if switch.oauth_to_email() {
        tracing::Span::current().record("branch", "oauth_to_email");
        return switch_oauth_to_email(state, request, session, &switch).await;
    }
    if switch.email_to_ldap() {
        tracing::Span::current().record("branch", "email_to_ldap");
        return switch_from_email(state, request, &switch.email).await;
    }
    if switch.ldap_to_email() {
        tracing::Span::current().record("branch", "ldap_to_email");
        return switch_ldap_to_email(state, request, &switch.email).await;
    }

    // `c.SetInvalidParam("switch_request")` — the same body id a decode failure gets, so a client
    // cannot tell a malformed body from an unsupported pair of services.
    ApiError::invalid_param("switch_request").into_response()
}

/// The two branches whose next step after `GetUserByEmail` writes: `email → oauth` and
/// `email → ldap`. The 404 is served and everything past it is Go's.
async fn switch_from_email(state: AppState, request: Request, email: &str) -> Response {
    if let Err(err) = state.app.get_user_by_email(email).await {
        return ApiError::from(err).into_response();
    }
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

/// Port of `App.SwitchOAuthToEmail` (app/oauth.go:1208), up to the password change.
///
/// Seven refusals in a fixed order, and the order is observable: an unknown address is a 404
/// before the id-mismatch 403, so a caller cannot enumerate addresses by watching which refusal
/// it gets — it gets the 404 either way.
async fn switch_oauth_to_email(
    state: AppState,
    request: Request,
    session: OptionalSession,
    switch: &SwitchRequest,
) -> Response {
    // `c.SessionRequired()` — an empty session is the 401, and `Session().UserId == ""` is the
    // test Go makes. A bad token reaches here as no session at all, not as a rejection.
    let Some(session) = session.0 else {
        return ApiError::from(AppError::new(
            "",
            "api.context.session_expired.app_error",
            None,
            "UserRequired".to_owned(),
            401,
        ))
        .into_response();
    };
    if session.is_oauth {
        return oauth_app_refusal(&AuthenticatedSession(session));
    }

    match state.app.license_state().await {
        // `License() != nil && !ExperimentalEnableAuthenticationTransfer` — the flag is not
        // ported, so a licence means this decision is not ours to take.
        Ok(LicenseState::Licensed) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(_) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    if let Some(refusal) = email_signin_refusals(&state, "SwitchOAuthToEmail") {
        return refusal;
    }

    let user = match state.app.get_user_by_email(&switch.email).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if user.is_magic_link_enabled() {
        return ApiError::from(AppError::new(
            "SwitchOAuthToEmail",
            "api.user.oauth_to_email.magic_link.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    // The address in the body must belong to the caller. An administrator cannot use this route
    // on somebody else's account — `updateUserAuth` is the route that can.
    if user.id != session.user_id {
        return ApiError::from(AppError::new(
            "SwitchOAuthToEmail",
            "api.user.oauth_to_email.context.app_error",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    // Note this reads the *stored* `AuthService`, not the body's `current_service`: a request
    // claiming `gitlab` for an account that authenticates by password is refused here, having
    // been routed by the claim.
    if !user.is_oauth_user() && !user.is_saml_user() {
        return ApiError::from(AppError::new(
            "SwitchOAuthToEmail",
            "api.user.oauth_to_email.not_oauth_user.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    // `UpdatePassword` hashes and writes, then sends a sign-in-change e-mail and revokes every
    // session. Forwarded before the first of those, which is the constraint this route is under.
    tracing::Span::current().record("forwarded", true);
    proxy::forward_to_go(State(state), request).await
}

/// Port of `App.SwitchLdapToEmail` (app/ldap.go:149) — the whole branch, because it cannot get
/// past `ldapInterface == nil` in this build. See [`switch_account_type`].
async fn switch_ldap_to_email(state: AppState, request: Request, email: &str) -> Response {
    match state.app.license_state().await {
        Ok(LicenseState::Licensed) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(_) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    // Both ids say `SwitchEmailToLdap`, in `SwitchLdapToEmail`. Go's copy-paste, reproduced —
    // `where` is not on the wire, so this is for the log and for a reader who greps for the id.
    if let Some(refusal) = email_signin_refusals(&state, "SwitchEmailToLdap") {
        return refusal;
    }

    let user = match state.app.get_user_by_email(email).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if user.auth_service != USER_AUTH_SERVICE_LDAP {
        return ApiError::from(AppError::new(
            "SwitchLdapToEmail",
            "api.user.ldap_to_email.not_ldap_account.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    // `ldapInterface == nil || user.AuthData == nil`. The first disjunct is always true here, so
    // the second cannot be observed — an `ldap` row with a NULL `AuthData` and one with a value
    // get the same 501. Written as Go writes it so that porting the interface changes one line.
    let _ = request;
    ApiError::from(AppError::new(
        "SwitchLdapToEmail",
        "api.user.ldap_to_email.not_available.app_error",
        None,
        String::new(),
        501,
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four predicates route a `saml → ldap` request to the 400 rather than to either LDAP
    /// branch, and `email → email` to it as well. Both are in `model`, but the *consequence* —
    /// that this handler's fall-through is reachable — is this file's.
    #[test]
    fn the_unroutable_pairs_fall_through() {
        for (current, new) in [
            ("saml", "ldap"),
            ("ldap", "saml"),
            ("email", "email"),
            ("email", "bogus"),
            ("", ""),
        ] {
            let switch = SwitchRequest {
                current_service: current.to_owned(),
                new_service: new.to_owned(),
                ..SwitchRequest::default()
            };
            assert!(
                !switch.email_to_oauth()
                    && !switch.oauth_to_email()
                    && !switch.email_to_ldap()
                    && !switch.ldap_to_email(),
                "{current} -> {new} must reach the invalid-param answer"
            );
        }
    }

    /// And the ten pairs that do route, so that a mutation narrowing one predicate is caught
    /// without a stack.
    #[test]
    fn the_routable_pairs_each_pick_one_branch() {
        for new in ["saml", "gitlab", "google", "office365", "openid"] {
            let out = SwitchRequest {
                current_service: "email".to_owned(),
                new_service: new.to_owned(),
                ..SwitchRequest::default()
            };
            assert!(out.email_to_oauth(), "email -> {new}");
            let back = SwitchRequest {
                current_service: new.to_owned(),
                new_service: "email".to_owned(),
                ..SwitchRequest::default()
            };
            assert!(back.oauth_to_email(), "{new} -> email");
        }
    }
}
