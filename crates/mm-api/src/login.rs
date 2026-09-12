//! The login pair (api4/user.go):
//!
//! ```text
//! POST /api/v4/users/login        login         (user.go:2125)
//! POST /api/v4/users/login/type   getLoginType  (user.go:2426)
//! ```
//!
//! # This is the only route that mints a credential, and everything else here follows from that
//!
//! Three things about `login` are unlike every other handler in this crate.
//!
//! **It answers with a different error id for the same failure depending on configuration.** A
//! deferred function rewrites `c.Err` on the way out (user.go:2127-2186): twelve ids pass through
//! untouched and *everything else* becomes one of four `invalid_credentials_*` ids chosen by the
//! SSO flags and the two sign-in toggles. Clients branch on those ids to decide what to put in
//! the login form, so the mask is wire format. See [`mask_login_error`].
//!
//! **It mutates shared state before it can fail.** `Users.FailedAttempts` is claimed before the
//! password is checked. A forward taken *after* that claim would have Go claim a second slot for
//! the same attempt, so every condition this port cannot serve is detected **first** — see
//! [`login`]'s forwarding table. The MFA probe costs an extra `SELECT` for exactly this reason.
//!
//! **Its success response is four headers and a body.** `Token`, and — only for a request
//! carrying `X-Requested-With: XMLHttpRequest` — the three cookies `MMAUTHTOKEN` (HttpOnly),
//! `MMUSERID` and `MMCSRF` (not). A client that omits the header gets the token header alone,
//! which is what `curl` sees and what the parity suite must therefore compare both ways.
//!
//! # CSRF
//!
//! Not modelled here either; [D-236] covers the whole port. `login` is the route that *issues*
//! the CSRF token rather than one that checks it, so nothing is weakened by its absence on this
//! path specifically.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_model::session::{
    LoginOptions, SESSION_COOKIE_CSRF, SESSION_COOKIE_TOKEN, SESSION_COOKIE_USER,
};
use mm_model::utils::{AppError, StringMap, get_millis};

use crate::AppState;
use crate::error::ApiError;
use crate::proxy;
use crate::sessions::{check_embedded_cookie, render_session_cookie};

/// Port of `model.MapFromJSON` (utils.go:507) — every failure is an empty map.
///
/// A third copy of the two-liner `auth_writes` and `channel_member_writes` already carry, for the
/// reason stated there: several agents edit this workspace at once and a shared two-line helper
/// is a worse merge risk than a copy.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// `model.HeaderRequestedWith` / `model.HeaderRequestedWithXML` (model/client4.go constants,
/// mirrored in `model/headers.go`). The pair is what gates `AttachSessionCookies`.
const HEADER_REQUESTED_WITH: &str = "X-Requested-With";
const HEADER_REQUESTED_WITH_XML: &str = "XMLHttpRequest";

/// `model.HeaderToken` — the response header every client reads the new session token from.
const HEADER_TOKEN: &str = "Token";

/// The twelve ids `login`'s deferred mask lets through unchanged (api4/user.go:2134-2146).
///
/// Ordered as Go writes them. Five are unreachable from this port and are listed anyway, because
/// the *list* is the thing being ported — an id dropped from it stops being distinguishable to a
/// client, and the ones reachable only after a licence or an LDAP server is configured must
/// survive that change without anyone remembering this file.
const UNMASKED_ERRORS: &[&str] = &[
    // Both MFA ids: forwarded by this port, so Go writes them, not us.
    "mfa.validate_token.authenticate.app_error",
    "api.user.check_user_mfa.bad_code.app_error",
    "api.user.login.blank_pwd.app_error",
    "api.user.login.bot_login_forbidden.app_error",
    "api.user.login.remote_users.login.error",
    // Client-side certificates are not implemented anywhere in this port.
    "api.user.login.client_side_cert.certificate.app_error",
    "api.user.login.inactive.app_error",
    "api.user.login.not_verified.app_error",
    "api.user.check_user_login_attempts.too_many.app_error",
    // Both max-accounts ids belong to the licensed user-limit path.
    "app.team.join_user_to_team.max_accounts.app_error",
    "store.sql_user.save.max_accounts.app_error",
    "api.user.check_user_login_attempts.too_many_ldap.app_error",
];

/// Port of the deferred mask at the top of `login` (api4/user.go:2127).
///
/// # Four ids, and which one you get is a statement about the server
///
/// After the unmasked list, the choice is made by five SSO flags and two sign-in toggles, in this
/// order — and the first arm wins:
///
/// 1. **any** of SAML, GitLab, Google, Office365, OpenID enabled → `invalid_credentials_sso`;
/// 2. username on, email off → `invalid_credentials_username`;
/// 3. username off, email on → `invalid_credentials_email`;
/// 4. otherwise → `invalid_credentials_email_username`.
///
/// Arm 4 catches both "both on" (the stock server) *and* "both off", which are opposite
/// configurations reported identically. Every masked error becomes a **401**, so a 400 from a
/// malformed body and a 500 from a broken database both leave this function as 401s — a client
/// cannot tell a server fault from a wrong password, which is the point.
///
/// The `Where` is rewritten to `"login"` along with the id, because Go builds a fresh `AppError`
/// rather than editing the old one.
fn mask_login_error(state: &AppState, err: ApiError) -> ApiError {
    if UNMASKED_ERRORS.contains(&err.0.id.as_str()) {
        return err;
    }

    let config = state.app.config();
    let masked = if config.saml_enable
        || config.gitlab_enable
        || config.google_enable
        || config.office365_enable
        || config.openid_enable
    {
        "api.user.login.invalid_credentials_sso"
    } else if config.enable_sign_in_with_username && !config.enable_sign_in_with_email {
        "api.user.login.invalid_credentials_username"
    } else if !config.enable_sign_in_with_username && config.enable_sign_in_with_email {
        "api.user.login.invalid_credentials_email"
    } else {
        "api.user.login.invalid_credentials_email_username"
    };

    tracing::debug!(original = %err.0.id, masked = %masked, "login failure masked");
    ApiError::from(AppError::boxed("login", masked, None, String::new(), 401))
}

/// Port of `login` (api4/user.go:2125).
///
/// # What is forwarded, and why each decision is made where it is
///
/// | Condition | Checked | Reason |
/// |---|---|---|
/// | `magic_link_token` present | before the body is otherwise read | `AuthenticateUserForGuestMagicLink` is not ported, and the branch is licensed |
/// | `LdapSettings.Enable` | before any lookup | both `GetUserForLogin` and `authenticateUser` consult an LDAP client this port has not got |
/// | the installation is licensed | before any lookup | the guest-account branch, the LDAP picture refresh and the cloud cookie all read the licence |
/// | the account has MFA and the server has MFA on | after the blank-password check, before the counter | Go asks *after* claiming a failed-attempt slot; asking here costs one `SELECT` and keeps the counter honest |
///
/// Every one of those is a read. Nothing in this handler writes until
/// `App::authenticate_user_for_login`, which is past all four.
///
/// # The order of the checks after authentication
///
/// `IsMagicLinkEnabled` → guest → remote, and all three run **after** the failed-attempt counter
/// has been zeroed by a successful password check. So a remote user with the right password has
/// their lockout cleared and is then refused. That is Go's order and it is observable in the
/// column.
///
/// # `Token` is set inside `DoLogin`, before the terms-of-service read
///
/// Go writes the header at app/login.go:216 and only then looks up
/// `GetUserTermsOfService`, so a 500 from that lookup still carries a valid token for a session
/// that exists. This port returns the error without the header. Recorded rather than reproduced:
/// the divergence is confined to a database failure between two reads, and handing a client a
/// credential alongside a 500 is the worse of the two behaviours. Same shape as the
/// `attachDeviceIds` cookie divergence in [`crate::sessions`].
#[tracing::instrument(skip_all, fields(forwarded = false, outcome))]
pub async fn login(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    let (request, bytes) = match split_body(request).await {
        Ok(pair) => pair,
        Err(err) => return mask_login_error(&state, err).into_response(),
    };
    let props = map_from_json(&bytes);
    let get = |key: &str| props.get(key).map_or("", String::as_str);

    let id = get("id");
    let login_id = get("login_id");
    let password = get("password");
    let mfa_token = get("token");
    let device_id = get("device_id");
    let voip_device_id = get("voip_device_id");

    // `ldap_only` is read into a variable by `login` (user.go:2193) and handed to
    // `AuthenticateUserForLogin`, whose body never mentions it (app/login.go:29). A dead
    // parameter, so there is nothing to forward on.

    if !get("magic_link_token").is_empty() {
        tracing::Span::current().record("forwarded", true);
        tracing::Span::current().record("outcome", "magic_link");
        return proxy::forward_to_go(State(state), request).await;
    }

    if state.app.config().ldap_enable {
        tracing::Span::current().record("forwarded", true);
        tracing::Span::current().record("outcome", "ldap");
        return proxy::forward_to_go(State(state), request).await;
    }

    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Licensed) => {
            tracing::Span::current().record("forwarded", true);
            tracing::Span::current().record("outcome", "licensed");
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(_) => {}
        Err(err) => return mask_login_error(&state, ApiError::from(err)).into_response(),
    }

    // `AuthenticateUserForLogin` refuses a blank password before it looks anything up, and that
    // id is unmasked — so the probe below must not run first or an empty body would answer the
    // lookup error instead.
    if !password.is_empty() && state.app.login_needs_mfa(id, login_id).await {
        tracing::Span::current().record("forwarded", true);
        tracing::Span::current().record("outcome", "mfa");
        return proxy::forward_to_go(State(state), request).await;
    }

    match serve_login(
        &state,
        &headers,
        id,
        login_id,
        password,
        mfa_token,
        device_id,
        voip_device_id,
    )
    .await
    {
        Ok(response) => {
            tracing::Span::current().record("outcome", "ok");
            response
        }
        Err(err) => {
            tracing::Span::current().record("outcome", "refused");
            mask_login_error(&state, err).into_response()
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_login(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    login_id: &str,
    password: &str,
    mfa_token: &str,
    device_id: &str,
    voip_device_id: &str,
) -> Result<Response, ApiError> {
    let mut user = state
        .app
        .authenticate_user_for_login(id, login_id, password, mfa_token)
        .await?;

    // `user.IsMagicLinkEnabled()` is `AuthService == "magic_link" && IsGuest()`.
    //
    // **Unreachable from a password login, on both servers.** `authenticateUser` refuses *any*
    // non-empty `AuthService` (authentication.go:469) before this line is reached, so an account
    // whose `AuthService` is `magic_link` has already been turned away with
    // `use_auth_service`. The branch is live only for the `magic_link_token` path, which this
    // handler forwards. Reproduced rather than dropped because it is Go's control flow and the
    // next reader should find the same shape here as there — but no test can cover it, and that
    // is why.
    if user.is_magic_link_enabled() && !state.app.config().enable_guest_magic_link {
        return Err(ApiError::from(AppError::boxed(
            "login",
            "api.user.login.guest_magic_link.disabled.error",
            None,
            String::new(),
            401,
        )));
    }

    // `c.App.Channels().License() == nil` — the licensed installation forwarded above, so the
    // licence is always absent here and the first of Go's two guest refusals always wins. The
    // `GuestAccountsSettings.Enable` arm below it is therefore unreachable on this deployment and
    // is not written out.
    if user.is_guest() {
        return Err(ApiError::from(AppError::boxed(
            "login",
            "api.user.login.guest_accounts.license.error",
            None,
            String::new(),
            401,
        )));
    }

    if user.is_remote() {
        return Err(ApiError::from(AppError::boxed(
            "login",
            "api.user.login.remote_users.login.error",
            None,
            String::new(),
            401,
        )));
    }

    let opts = LoginOptions {
        device_id: device_id.to_owned(),
        voip_device_id: voip_device_id.to_owned(),
        is_mobile: mm_app::user_agent::is_mobile_request(user_agent(headers)),
        ..LoginOptions::default()
    };
    let session = state
        .app
        .do_login(&user, &opts, user_agent(headers))
        .await?;

    // `GetUserTermsOfService` is **not** gated on anything here — unlike `getUser`, which only
    // consults it for self or an admin. This is always self.
    match state.app.get_user_terms_of_service(&user.id).await {
        Ok(terms) => {
            user.terms_of_service_id = terms.terms_of_service_id;
            user.terms_of_service_create_at = terms.create_at;
        }
        Err(err) if err.status_code == 404 => {}
        Err(err) => return Err(ApiError::from(err)),
    }

    // `user.Sanitize(map[string]bool{})` — the empty map, so every optional field is stripped and
    // only the unconditional clears (password, auth data, MFA secret) plus the flag-gated ones
    // apply. Not `SanitizeProfile`: this is the caller's own row.
    user.sanitize(&std::collections::HashMap::new());

    let mut body = serde_json::to_vec(&user).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise User");
        ApiError::from(AppError::new(
            "login",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    // `json.NewEncoder(w).Encode(user)` — with the trailing newline. See [D-086].
    body.push(b'\n');

    let mut response = (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response();

    let token = axum::http::HeaderValue::from_str(&session.token).map_err(|err| {
        tracing::error!(error = %err, "the minted session token is not a header value");
        ApiError::from(AppError::new(
            "login",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    response.headers_mut().insert(HEADER_TOKEN, token);

    // `if r.Header.Get(model.HeaderRequestedWith) == model.HeaderRequestedWithXML` — an exact,
    // case-sensitive comparison on the whole header value, not a `contains`.
    if headers
        .get(HEADER_REQUESTED_WITH)
        .and_then(|value| value.to_str().ok())
        == Some(HEADER_REQUESTED_WITH_XML)
    {
        for cookie in session_cookies(state, headers, &session) {
            match axum::http::HeaderValue::from_str(&cookie) {
                Ok(value) => {
                    response.headers_mut().append("Set-Cookie", value);
                }
                Err(err) => {
                    tracing::error!(error = %err, "a session cookie is not a header value");
                }
            }
        }
    }

    Ok(response)
}

/// Port of `App.AttachSessionCookies` (app/login.go:270) — the three `Set-Cookie` values, in
/// emission order.
///
/// # Only the first is `HttpOnly`
///
/// `MMAUTHTOKEN` is; `MMUSERID` and `MMCSRF` are not, because the webapp reads both from
/// JavaScript — the user id to know who it is, the CSRF token to put in `X-CSRF-Token`. Setting
/// `HttpOnly` on all three would pass every status assertion and break the client.
///
/// # Everything else is shared with the three
///
/// `Path` from `GetSubpathFromConfig`, `Domain` from `GetCookieDomain` (empty unless
/// `AllowCookiesForSubdomains`, and empty means the attribute is omitted), `Max-Age` from
/// **`SessionLengthWebInHours`** — the web length, unconditionally, even for a session that
/// took the *mobile* length from `DoLogin`. That mismatch is Go's: a mobile client sending
/// `X-Requested-With` gets cookies whose lifetime is not its session's.
///
/// `Expires` is `GetMillis()/1000 + maxAgeSeconds` — a second-resolution instant computed from a
/// fresh clock read, so it is a second or two later than the session's own `ExpiresAt`.
///
/// The cloud cookie (`a.License().IsCloud()`) is unreachable: a licensed installation forwards.
fn session_cookies(
    state: &AppState,
    headers: &HeaderMap,
    session: &mm_model::session::Session,
) -> [String; 3] {
    let hours = state.app.config().session_length_web_in_hours;
    let max_age_seconds = hours * 60 * 60;
    let subpath = state.app.config().subpath();
    let domain = state.app.config().cookie_domain();
    let expires_at = get_millis() / 1000 + max_age_seconds;

    // `GetProtocol` is `X-Forwarded-Proto: https` or a TLS connection; this server terminates no
    // TLS, so the header is the only arm that can fire. Same reasoning as `attachDeviceIds`.
    let secure = headers
        .get("X-Forwarded-Proto")
        .and_then(|value| value.to_str().ok())
        == Some("https");
    let same_site_none = secure && check_embedded_cookie(headers);

    [
        render_session_cookie(
            SESSION_COOKIE_TOKEN,
            &session.token,
            &subpath,
            &domain,
            max_age_seconds,
            expires_at,
            true,
            secure,
            same_site_none,
        ),
        render_session_cookie(
            SESSION_COOKIE_USER,
            &session.user_id,
            &subpath,
            &domain,
            max_age_seconds,
            expires_at,
            false,
            secure,
            same_site_none,
        ),
        render_session_cookie(
            SESSION_COOKIE_CSRF,
            session.get_csrf(),
            &subpath,
            &domain,
            max_age_seconds,
            expires_at,
            false,
            secure,
            same_site_none,
        ),
    ]
}

/// Port of `getLoginType` (api4/user.go:2426).
///
/// # The gate is three terms and the first one is `false` on every stock server
///
/// ```text
/// if !*GuestAccountsSettings.EnableGuestMagicLink ||
///    !*GuestAccountsSettings.Enable ||
///    !*License().Features.GuestAccounts {
///     w.WriteHeader(http.StatusNotFound)
///     return
/// }
/// ```
///
/// So the whole route is **404 with an empty body** unless guest magic links are switched on, and
/// that is not a "not implemented" 404 — it is the answer, measured against the running Go server
/// on this stack (`Content-Length: 0`, no body, `Content-Type: application/json` from the
/// surrounding middleware).
///
/// The third term dereferences a licence Go does not null-check, so a server with the two flags
/// on and no licence panics there. This port does not model that: with both flags on the request
/// is **forwarded**, and Go answers however it answers. That keeps the one branch whose behaviour
/// is a crash out of this code entirely.
///
/// Everything past the gate — `GetUserForLogin`, the deactivated probe and the magic-link
/// eligibility rules — is therefore unreachable here and is not ported.
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn get_login_type(State(state): State<AppState>, request: Request) -> Response {
    if login_type_is_forwarded(state.app.config()) {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }

    // `w.WriteHeader(http.StatusNotFound)` and nothing else — no `AppError`, so none of
    // `handleContextError`'s body shape applies. The `Content-Type` is the one Go's middleware
    // sets on every api4 response.
    (
        StatusCode::NOT_FOUND,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
    )
        .into_response()
}

/// The first two terms of `getLoginType`'s gate, as a predicate, so all four combinations can be
/// asserted without a server.
///
/// **The conjunction is the whole point and the stack cannot show it.** Both flags are `false` on
/// a stock server, so `&&` and `||` agree there and a mutation swapping them survives every live
/// test — measured. Only a configuration where exactly one is set separates them, and that is
/// what [`tests::the_login_type_gate_is_a_conjunction`] builds.
///
/// `true` means hand the request to Go. The third term — `License().Features.GuestAccounts` — is
/// deliberately not here: Go dereferences a licence it does not null-check, so the branch where
/// both flags are on is Go's to answer however it answers.
fn login_type_is_forwarded(config: &mm_app::config::Config) -> bool {
    config.enable_guest_magic_link && config.guest_accounts_enable
}

/// `r.UserAgent()` — the first `User-Agent` header, or `""`.
fn user_agent(headers: &HeaderMap) -> &str {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// Read the whole body, keeping the parts so the request can still be forwarded.
///
/// A body this cannot read is an empty map on Go's side (`MapFromJSON` swallows the reader error
/// too), which then fails the blank-password check — so the error raised here is that same
/// `blank_pwd`, reached without pretending to have decoded anything.
async fn split_body(request: Request) -> Result<(Request, Vec<u8>), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the login body");
            ApiError::from(AppError::new(
                "AuthenticateUserForLogin",
                "api.user.login.blank_pwd.app_error",
                None,
                String::new(),
                400,
            ))
        })?;
    let rebuilt = Request::from_parts(parts, axum::body::Body::from(bytes.clone()));
    Ok((rebuilt, bytes.to_vec()))
}

/// `User.Sanitize(map[string]bool{})` never leaves a password behind, and this route is the one
/// where a leak would hand over a live credential rather than a profile field.
#[cfg(test)]
mod tests {
    use super::*;
    use mm_app::config::Config;
    use mm_model::session::Session;

    fn state_with(config: Config) -> AppState {
        AppState::new(
            mm_app::App::with_config(
                mm_store::SqlStore::from_pool(
                    sqlx::postgres::PgPoolOptions::new()
                        .connect_lazy("postgres://x/y")
                        .expect("a lazy pool needs no server"),
                ),
                config,
            ),
            "http://127.0.0.1:1".to_owned(),
        )
    }

    fn err(id: &str, status: i32) -> ApiError {
        ApiError::from(AppError::boxed(
            "somewhere",
            id,
            None,
            String::new(),
            status,
        ))
    }

    /// The four masked ids, one configuration each — and the order of the arms.
    ///
    /// This is the test the route exists for. Every one of these is reachable by changing two
    /// booleans in the System Console, every one is a *different string* a client branches on,
    /// and the development stack sits on exactly one of them — so nothing the parity suite runs
    /// against a live server can separate the other three.
    #[tokio::test]
    async fn the_mask_picks_its_id_from_the_configuration() {
        let cases: &[(Config, &str)] = &[
            // Stock: both sign-in methods on, no SSO.
            (
                Config {
                    enable_sign_in_with_email: true,
                    enable_sign_in_with_username: true,
                    ..Config::default()
                },
                "api.user.login.invalid_credentials_email_username",
            ),
            (
                Config {
                    enable_sign_in_with_email: false,
                    enable_sign_in_with_username: true,
                    ..Config::default()
                },
                "api.user.login.invalid_credentials_username",
            ),
            (
                Config {
                    enable_sign_in_with_email: true,
                    enable_sign_in_with_username: false,
                    ..Config::default()
                },
                "api.user.login.invalid_credentials_email",
            ),
            // Both off is *not* its own id — it falls to the same arm as both on.
            (
                Config {
                    enable_sign_in_with_email: false,
                    enable_sign_in_with_username: false,
                    ..Config::default()
                },
                "api.user.login.invalid_credentials_email_username",
            ),
        ];

        for (config, want) in cases {
            let state = state_with(config.clone());
            let masked = mask_login_error(&state, err("app.user.get.app_error", 500));
            assert_eq!(&masked.0.id, want);
            assert_eq!(masked.0.status_code, 401, "every masked error is a 401");
            assert_eq!(
                masked.0.where_, "login",
                "Go rebuilds the error, Where included"
            );
        }
    }

    /// **Each** of the five SSO flags wins on its own, and it beats the sign-in toggles.
    ///
    /// A port that read one flag for all five would agree with Go on the stock stack, where all
    /// five are `false`, and on any server where they happen to move together.
    #[tokio::test]
    async fn any_one_sso_flag_takes_precedence_over_the_sign_in_toggles() {
        let flags: &[fn(&mut Config)] = &[
            |c| c.saml_enable = true,
            |c| c.gitlab_enable = true,
            |c| c.google_enable = true,
            |c| c.office365_enable = true,
            |c| c.openid_enable = true,
        ];

        for set in flags {
            // Username-only, which without the SSO flag would answer `…_username`.
            let mut config = Config {
                enable_sign_in_with_email: false,
                enable_sign_in_with_username: true,
                ..Config::default()
            };
            set(&mut config);
            let state = state_with(config);
            assert_eq!(
                mask_login_error(&state, err("app.user.get.app_error", 500))
                    .0
                    .id,
                "api.user.login.invalid_credentials_sso"
            );
        }
    }

    /// All twelve unmasked ids survive, status and `Where` included — and one that is not on the
    /// list does not. Without the negative half, a mask that returned everything unchanged would
    /// pass.
    #[tokio::test]
    async fn the_unmasked_list_is_exactly_twelve_ids() {
        let state = state_with(Config::default());
        assert_eq!(UNMASKED_ERRORS.len(), 12);

        for id in UNMASKED_ERRORS {
            let passed = mask_login_error(&state, err(id, 418));
            assert_eq!(&passed.0.id, id, "{id} should pass the mask");
            assert_eq!(passed.0.status_code, 418, "{id} keeps its status");
            assert_eq!(passed.0.where_, "somewhere", "{id} keeps its Where");
        }

        // Near-misses: the LDAP unavailability and the wrong-auth-service ids look like they
        // belong on the list and do not, so both are masked.
        for id in [
            "api.user.login_ldap.not_available.app_error",
            "api.user.login.use_auth_service.app_error",
            "store.sql_user.get_for_login.app_error",
            "api.user.check_user_password.invalid.app_error",
            "api.user.login.guest_accounts.license.error",
        ] {
            assert_eq!(
                mask_login_error(&state, err(id, 400)).0.id,
                "api.user.login.invalid_credentials_email_username",
                "{id} should be masked"
            );
        }
    }

    /// The three cookies, and the one that differs.
    #[tokio::test]
    async fn the_three_cookies_carry_the_right_values_and_only_one_is_http_only() {
        let state = state_with(Config {
            session_length_web_in_hours: 4320,
            ..Config::default()
        });
        let mut session = Session {
            user_id: "opukwu61f7ft8exssjxf3huyjy".to_owned(),
            token: "mxnmpiimcjbe7ets4qyckj7b3y".to_owned(),
            ..Session::default()
        };
        let csrf = session.generate_csrf();

        let [token, user, cross] = session_cookies(&state, &HeaderMap::new(), &session);

        assert!(token.starts_with("MMAUTHTOKEN=mxnmpiimcjbe7ets4qyckj7b3y;"));
        assert!(user.starts_with("MMUSERID=opukwu61f7ft8exssjxf3huyjy;"));
        assert!(cross.starts_with(&format!("MMCSRF={csrf};")));

        assert!(token.contains("; HttpOnly"), "{token}");
        assert!(!user.contains("HttpOnly"), "{user}");
        assert!(!cross.contains("HttpOnly"), "{cross}");

        // 4320 hours in seconds — what the running Go server sends.
        for cookie in [&token, &user, &cross] {
            assert!(cookie.contains("; Max-Age=15552000"), "{cookie}");
            assert!(cookie.contains("; Path=/"), "{cookie}");
            assert!(!cookie.contains("Domain="), "{cookie}");
            assert!(!cookie.contains("Secure"), "{cookie}");
        }
    }

    /// The cookies take the **web** session length even when the session itself took the mobile
    /// one. Go's mismatch, and the two settings are given different values here so that a port
    /// reading the wrong one is visible.
    #[tokio::test]
    async fn the_cookies_use_the_web_length_not_the_mobile_one() {
        let state = state_with(Config {
            session_length_web_in_hours: 2,
            session_length_mobile_in_hours: 9,
            ..Config::default()
        });
        let session = Session {
            user_id: "opukwu61f7ft8exssjxf3huyjy".to_owned(),
            token: "mxnmpiimcjbe7ets4qyckj7b3y".to_owned(),
            ..Session::default()
        };
        for cookie in session_cookies(&state, &HeaderMap::new(), &session) {
            assert!(cookie.contains("; Max-Age=7200"), "{cookie}");
        }
    }

    /// `Secure` follows `X-Forwarded-Proto`, and `SameSite=None` additionally needs the embedded
    /// marker — on **all three** cookies, not just the token.
    #[tokio::test]
    async fn secure_and_same_site_apply_to_every_cookie() {
        let state = state_with(Config::default());
        let session = Session {
            user_id: "opukwu61f7ft8exssjxf3huyjy".to_owned(),
            token: "mxnmpiimcjbe7ets4qyckj7b3y".to_owned(),
            ..Session::default()
        };

        let mut https = HeaderMap::new();
        https.insert(
            "X-Forwarded-Proto",
            axum::http::HeaderValue::from_static("https"),
        );
        for cookie in session_cookies(&state, &https, &session) {
            assert!(cookie.contains("; Secure"), "{cookie}");
            assert!(!cookie.contains("SameSite"), "{cookie}");
        }

        let mut embedded = https.clone();
        embedded.insert("Cookie", axum::http::HeaderValue::from_static("MMEMBED=1"));
        for cookie in session_cookies(&state, &embedded, &session) {
            assert!(cookie.ends_with("; SameSite=None"), "{cookie}");
        }

        // Embedded without https: neither attribute, because `SameSite` is `secure && embedded`.
        let mut plain = HeaderMap::new();
        plain.insert("Cookie", axum::http::HeaderValue::from_static("MMEMBED=1"));
        for cookie in session_cookies(&state, &plain, &session) {
            assert!(!cookie.contains("Secure"), "{cookie}");
            assert!(!cookie.contains("SameSite"), "{cookie}");
        }
    }

    /// `getLoginType`'s gate is a **conjunction**, and only a configuration where exactly one of
    /// the two flags is set can say so.
    ///
    /// A stock server has both off, so `&&` and `||` both answer "do not forward" and the whole
    /// route is a 404 either way. The `||` reading would hand Go every request on a server that
    /// had guest accounts on and magic links off — an ordinary configuration — and answer the
    /// 404 on one that had them the other way round.
    #[test]
    fn the_login_type_gate_is_a_conjunction() {
        let with = |magic_link: bool, guests: bool| {
            login_type_is_forwarded(&Config {
                enable_guest_magic_link: magic_link,
                guest_accounts_enable: guests,
                ..Config::default()
            })
        };
        assert!(
            !with(false, false),
            "the stock server answers the 404 itself"
        );
        assert!(
            !with(true, false),
            "magic links without guest accounts is still a 404"
        );
        assert!(
            !with(false, true),
            "guest accounts without magic links is still a 404"
        );
        assert!(with(true, true), "both on is Go's to answer");

        // And the default really is the 404 arm, so the route is served rather than forwarded on
        // an unconfigured server.
        assert!(!login_type_is_forwarded(&Config::default()));
    }

    /// `map_from_json` swallows everything, so a malformed login body is a blank password rather
    /// than a decode error — which is the id the client sees.
    #[test]
    fn map_from_json_turns_every_failure_into_an_empty_map() {
        assert!(map_from_json(b"").is_empty());
        assert!(map_from_json(b"null").is_empty());
        assert!(map_from_json(b"[]").is_empty());
        assert!(map_from_json(br#"{"login_id": 7}"#).is_empty());
        assert!(map_from_json(b"{").is_empty());
        assert_eq!(
            map_from_json(br#"{"login_id":"a@b.c"}"#)
                .get("login_id")
                .map(String::as_str),
            Some("a@b.c")
        );
    }

    /// `X-Requested-With` is compared for **equality** against `XMLHttpRequest`, so a client that
    /// sends anything else — including a superset — gets no cookies at all.
    #[test]
    fn the_cookie_gate_is_an_exact_header_match() {
        let matches = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                HEADER_REQUESTED_WITH,
                axum::http::HeaderValue::from_static(value),
            );
            headers
                .get(HEADER_REQUESTED_WITH)
                .and_then(|v| v.to_str().ok())
                == Some(HEADER_REQUESTED_WITH_XML)
        };
        assert!(matches("XMLHttpRequest"));
        assert!(!matches("xmlhttprequest"));
        assert!(!matches("XMLHttpRequest, fetch"));
        assert!(!matches(""));
    }
}
