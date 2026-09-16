//! Port of `app.ParseAuthTokenFromRequest` (channels/app/authentication.go:493) and the session
//! half of `web.Context.ApiSessionRequired`.

use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use mm_model::session::Session;

use crate::AppState;
use crate::error::ApiError;

/// `model.SessionCookieToken`.
const SESSION_COOKIE_TOKEN: &str = "MMAUTHTOKEN";
/// `model.HeaderBearer`. Go compares the first six bytes upper-cased.
const HEADER_BEARER: &str = "BEARER";
/// `model.HeaderToken`. Go compares the first five bytes lower-cased.
const HEADER_TOKEN: &str = "token";
/// `model.HeaderCsrfToken`.
const HEADER_CSRF_TOKEN: &str = "X-CSRF-Token";
/// `model.HeaderRequestedWith`.
const HEADER_REQUESTED_WITH: &str = "X-Requested-With";
/// `model.HeaderRequestedWithXML`.
const HEADER_REQUESTED_WITH_XML: &[u8] = b"XMLHttpRequest";
/// Go truncates the returned token at 50 bytes in a deferred block. See [`parse_auth_token`].
const MAX_TOKEN_LEN: usize = 50;

/// Where the token was found. Port of `app.TokenLocation`, restricted to the locations parsed
/// here — the cloud and remote-cluster headers are not (D-081).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLocation {
    Cookie,
    Header,
    QueryString,
}

/// Port of `ParseAuthTokenFromRequest` (authentication.go:493).
///
/// # The cookie wins
///
/// Go checks the cookie **before** the `Authorization` header and returns immediately if one is
/// present. A browser session therefore beats an explicitly supplied bearer token on the same
/// request. That ordering is reproduced deliberately: reversing it would authenticate some
/// requests as a different user than the Go server does, which during a migration means two
/// servers disagreeing about who is calling.
///
/// # The 50-byte truncation is real
///
/// Go's `defer` block truncates the *named return value*, not just a log line, so a token longer
/// than 50 bytes is returned truncated and will not match any row. Session tokens are 26
/// characters so nothing reachable hits it, but it is behaviour rather than logging and is
/// reproduced rather than tidied away.
pub fn parse_auth_token(parts: &Parts) -> Option<(String, TokenLocation)> {
    let (token, location) = parse_auth_token_untruncated(parts)?;
    let token = match token.char_indices().nth(MAX_TOKEN_LEN) {
        // Go slices bytes; slicing a multi-byte character mid-way would panic in Rust, so the cut
        // is made at the nearest character boundary at or before the limit. No reachable token is
        // non-ASCII, and a token that long is already guaranteed not to match.
        Some((byte_idx, _)) => token[..byte_idx].to_owned(),
        None => token,
    };
    Some((token, location))
}

fn parse_auth_token_untruncated(parts: &Parts) -> Option<(String, TokenLocation)> {
    // 1. The cookie, checked first — see the note above.
    if let Some(cookie_header) = parts.headers.get(axum::http::header::COOKIE)
        && let Ok(cookie_header) = cookie_header.to_str()
    {
        for pair in cookie_header.split(';') {
            let pair = pair.trim_start();
            if let Some(rest) = pair.strip_prefix(SESSION_COOKIE_TOKEN)
                && let Some(value) = rest.strip_prefix('=')
            {
                return Some((value.to_owned(), TokenLocation::Cookie));
            }
        }
    }

    let auth_header = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    // 2. `Authorization: Bearer <token>`. Go tests `len > 6` and upper-cases the first six bytes,
    //    then slices from index 7 — so the separator byte at index 6 is skipped without being
    //    checked. A header of `BEARERx<token>` is accepted by Go and therefore here.
    if auth_header.len() > 6 && auth_header[..6].to_ascii_uppercase() == HEADER_BEARER {
        return Some((auth_header[7..].to_owned(), TokenLocation::Header));
    }

    // 3. `Authorization: token <token>` — the OAuth form, same off-by-one shape.
    if auth_header.len() > 5 && auth_header[..5].to_ascii_lowercase() == HEADER_TOKEN {
        return Some((auth_header[6..].to_owned(), TokenLocation::Header));
    }

    // 4. `?access_token=`.
    if let Some(query) = parts.uri.query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("access_token=")
                && !value.is_empty()
            {
                return Some((value.to_owned(), TokenLocation::QueryString));
            }
        }
    }

    None
}

/// An authenticated session, extracted the way `ApiSessionRequired` does it.
///
/// Used as an axum extractor: a handler taking `AuthenticatedSession` cannot be reached without a
/// valid session, which is the same guarantee Go gets from wrapping the handler.
#[derive(Debug, Clone)]
pub struct AuthenticatedSession(pub Session);

/// The extractor's rejection: an [`ApiError`], plus whether Go would also have cleared the session
/// cookie on the way out.
///
/// A separate type rather than a field on `ApiError` because the cookie belongs to **one branch of
/// one extractor** — `handlers.go:278` — and every other 401 in the tree must not grow one. Go
/// draws the same line: `RemoveSessionCookie` is called there and in the logout handler, not by
/// the error renderer.
pub struct SessionRejection {
    error: ApiError,
    /// Port of `c.RemoveSessionCookie(w, r)` (web/context.go:180) — `Some(subpath)` when Go would
    /// clear the cookie, carrying the `Path` it would scope it to.
    clear_session_cookie: Option<String>,
}

impl SessionRejection {
    /// The `TokenRequired` branch (web/context.go:154): no token was presented at all.
    ///
    /// **Go does not clear the cookie here**, and that is measured rather than read: a request
    /// with no `Authorization` header and no cookie gets a bare 401 from the running server,
    /// while the same request with a bad token gets a `Set-Cookie`. Nothing to clear is not the
    /// same as something to clear.
    /// `MfaRequired`'s refusal, as `c.Err`: the error unchanged, no cookie cleared.
    fn mfa_required(err: Box<mm_model::utils::AppError>) -> Self {
        Self {
            error: ApiError::from(err),
            clear_session_cookie: None,
        }
    }

    /// `checkCSRFToken` failed (web/handlers.go:296-300): the same generic 401 as a rejected
    /// token, and the cookie is cleared — Go calls `RemoveSessionCookie` on this branch too.
    ///
    /// Go's detailed error, `token=<token> Appears to be a CSRF attempt`, is wiped before it
    /// reaches the client unless `EnableDeveloper` is on; like the rest of this extractor, the
    /// token is never interpolated here.
    pub(crate) fn csrf_failed(subpath: String) -> Self {
        Self {
            error: ApiError::unauthenticated(),
            clear_session_cookie: Some(subpath),
        }
    }

    fn no_token() -> Self {
        Self {
            error: ApiError::unauthenticated(),
            clear_session_cookie: None,
        }
    }

    /// Port of `handlers.go:273-280` — what the web layer does with `GetSession`'s error.
    ///
    /// A named function rather than a closure inside the extractor, because the 500 arm is
    /// otherwise **unreachable from any test**: getting there needs the session store to fail,
    /// which a parity test cannot arrange against a healthy database. A mutation that cleared the
    /// cookie on a 500 survived the whole suite until this existed.
    ///
    /// `subpath` is a closure so the config is not consulted on the branch that does not need it.
    fn for_get_session_error(
        err: Box<mm_model::utils::AppError>,
        subpath: impl FnOnce() -> String,
    ) -> Self {
        if err.status_code == 500 {
            // Go keeps a 500 as-is (`c.Err = err`) and never reaches `RemoveSessionCookie`, which
            // lives in the `else if` below it. A database failure must not log the user out — and
            // it must not be reported to the client as a bad token either.
            Self {
                error: ApiError::from(err),
                clear_session_cookie: None,
            }
        } else {
            Self {
                error: ApiError::unauthenticated(),
                clear_session_cookie: Some(subpath()),
            }
        }
    }
}

/// Render `RemoveSessionCookie`'s cookie the way `net/http` writes it.
///
/// ```text
/// MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly
/// ```
///
/// Three details, all measured against the running Go server rather than inferred:
///
/// - **`MaxAge: -1` serialises as `Max-Age=0`**, not `-1`. Go maps negative to the literal zero
///   (net/http/cookie.go), which is the spelling browsers treat as "delete now".
/// - **An empty `Path` is omitted entirely**, header attribute and all. That is the shape a
///   SiteURL Go cannot parse produces — see [`mm_app::config::Config::subpath`] — so it is a
///   reachable case rather than a defensive one.
/// - **The attribute order is `Path`, `Max-Age`, `HttpOnly`**, which is the order `Cookie.String`
///   emits them in. Nothing parses cookies positionally, but the parity assertion compares the
///   header verbatim and it costs nothing to be right.
fn remove_session_cookie_header(subpath: &str) -> String {
    let path = sanitize_cookie_path(subpath);
    let mut cookie = format!("{SESSION_COOKIE_TOKEN}=");
    if !path.is_empty() {
        cookie.push_str("; Path=");
        cookie.push_str(&path);
    }
    cookie.push_str("; Max-Age=0; HttpOnly");
    cookie
}

/// Port of `sanitizeCookiePath` / `validCookiePathByte` (net/http/cookie.go:524).
///
/// Keeps `0x20..0x7f` except `;`. Go logs and drops the invalid bytes rather than refusing, and a
/// dropped byte here is far better than a rejected header: `axum` would otherwise turn a
/// pathological `SiteURL` into a 500 on the *authentication* path.
fn sanitize_cookie_path(value: &str) -> String {
    value
        .bytes()
        .filter(|&b| (0x20..0x7f).contains(&b) && b != b';')
        .map(char::from)
        .collect()
}

/// A plain error with no cookie to clear — every rejection but the rejected-token and CSRF ones.
impl From<ApiError> for SessionRejection {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            clear_session_cookie: None,
        }
    }
}

impl IntoResponse for SessionRejection {
    fn into_response(self) -> Response {
        let cookie = self
            .clear_session_cookie
            .map(|subpath| remove_session_cookie_header(&subpath));
        let mut response = self.error.into_response();
        if let Some(cookie) = cookie
            && let Ok(value) = axum::http::HeaderValue::from_str(&cookie)
        {
            response
                .headers_mut()
                .insert(axum::http::header::SET_COOKIE, value);
        }
        response
    }
}

/// Marks a route Go registers with `APIHandlerTrustRequester` or
/// `APISessionRequiredTrustRequester` — `TrustRequester: true` (api4/handlers.go), whose only
/// effect is to waive [`check_csrf_token`]. Inserted as a request extension by
/// `crate::trust_requester` on exactly those method routes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustRequester;

/// What [`check_csrf_token`] decided. Port of its `(checked, passed)` pair, minus the one
/// combination Go never returns (`checked == false, passed == true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CsrfCheck {
    /// Not a cookie, a `GET`, or a trusted requester: nothing was checked.
    NotNeeded,
    Passed,
    Failed,
}

/// The request half of `csrfCheckNeeded`: a cookie token, an untrusted route, a method other than
/// `GET`. The other half (a session resolved, no error yet) is the caller's position.
fn csrf_check_applies(parts: &Parts, location: TokenLocation) -> bool {
    location == TokenLocation::Cookie
        && parts.extensions.get::<TrustRequester>().is_none()
        && parts.method != Method::GET
}

/// Port of `Handler.checkCSRFToken` (web/handlers.go:508).
///
/// # When it runs
///
/// Only for a session that resolved, presented in the **cookie**, on a route that is not
/// `TrustRequester`, with a method that is not literally `GET`. So `HEAD`, `OPTIONS` and every
/// write are checked; a bearer token never is. The caller supplies "a session resolved and no
/// error was set" by calling this only on that path (`session != nil && c.Err == nil`).
///
/// # What passes
///
/// 1. `X-CSRF-Token` equal to the session's `csrf` prop. **Equal, including empty**: a session
///    whose props carry no `csrf` (`GetCSRF` answers `""`) passes a request with no header at all.
///    Every login session has one, so this is not how a browser gets through, but it is Go's
///    comparison and it is reproduced rather than tightened.
/// 2. Otherwise `X-Requested-With: XMLHttpRequest` — the pre-2019 webapp's signal — passes
///    **unless** `ExperimentalStrictCSRFEnforcement` is on. Either way Go logs it (debug when
///    lenient, warn when strict).
///
/// Anything else fails. The header values are compared as bytes, as Go's `Header.Get` does; a
/// value that is not UTF-8 is simply unequal rather than an error.
pub(crate) fn check_csrf_token(
    parts: &Parts,
    location: TokenLocation,
    session: &Session,
    strict: bool,
) -> CsrfCheck {
    if !csrf_check_applies(parts, location) {
        return CsrfCheck::NotNeeded;
    }

    let header = parts
        .headers
        .get(HEADER_CSRF_TOKEN)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    if header == session.get_csrf().as_bytes() {
        return CsrfCheck::Passed;
    }

    let requested_with = parts
        .headers
        .get(HEADER_REQUESTED_WITH)
        .map(|value| value.as_bytes());
    if requested_with == Some(HEADER_REQUESTED_WITH_XML) {
        if strict {
            tracing::warn!(
                path = parts.uri.path(),
                session_id = %session.id,
                user_id = %session.user_id,
                "CSRF Header check failed for request - Please upgrade your web application or custom app to set a CSRF Header"
            );
        } else {
            tracing::debug!(
                path = parts.uri.path(),
                session_id = %session.id,
                user_id = %session.user_id,
                "CSRF Header check failed for request - Please upgrade your web application or custom app to set a CSRF Header"
            );
            return CsrfCheck::Passed;
        }
    }

    CsrfCheck::Failed
}

/// [`check_csrf_token`] against this server's configuration, as the rejection Go answers.
pub(crate) fn enforce_csrf(
    parts: &Parts,
    state: &AppState,
    location: TokenLocation,
    session: &Session,
) -> Result<(), SessionRejection> {
    let config = state.app.config();
    match check_csrf_token(
        parts,
        location,
        session,
        config.experimental_strict_csrf_enforcement,
    ) {
        CsrfCheck::Failed => Err(SessionRejection::csrf_failed(config.subpath())),
        CsrfCheck::NotNeeded | CsrfCheck::Passed => Ok(()),
    }
}

/// `api.context.token_provided.app_error` (handlers.go:281): a *valid* non-OAuth session presented
/// as `?access_token=`. Go refuses it inside `ServeHTTP`, for every handler, before the handler
/// runs — a session token in a URL leaks into logs and `Referer` headers, and only OAuth tokens are
/// meant to travel that way.
pub(crate) fn token_provided_rejection() -> SessionRejection {
    ApiError::from(mm_model::utils::AppError::new(
        "ServeHTTP",
        "api.context.token_provided.app_error",
        None,
        // Go interpolates the token here; `wipe_detailed` blanks it before it reaches the client,
        // so the value never leaves the process either way.
        String::new(),
        401,
    ))
    .into()
}

/// The token half of `ServeHTTP` for a handler that takes **no** session extractor: the
/// query-string refusal and the CSRF check.
///
/// Go resolves any token it finds for every handler, whether or not the handler wants a session —
/// so `POST /users/login` with a live session cookie and no CSRF header is a 401 from Go before
/// `login` runs, and so is `POST /users/login/type?access_token=<a valid session token>`. A
/// handler here that never looks at the session would silently skip both, so it names this
/// extractor instead. It does nothing unless the request carries a token in the query string or is
/// a CSRF-checked one (cookie, not `GET`, not `TrustRequester`), and only then looks the session
/// up.
///
/// The lookup's failures follow `RequireSession: false` (handlers.go:273): a rejected token is not
/// an error and skips both checks (Go's `session` is nil), while a store failure is the 500 Go sets
/// as `c.Err`.
pub struct CsrfGuard;

impl FromRequestParts<AppState> for CsrfGuard {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some((token, location)) = parse_auth_token(parts) else {
            return Ok(CsrfGuard);
        };
        // Only a request one of the two checks applies to pays for the session lookup.
        if location != TokenLocation::QueryString && !csrf_check_applies(parts, location) {
            return Ok(CsrfGuard);
        }
        match state.app.get_session(&token).await {
            Ok(session) => {
                // `else if`: a refused query token never reaches the CSRF check. It could not
                // fail it anyway — the check applies only to a cookie.
                if !session.is_oauth && location == TokenLocation::QueryString {
                    return Err(token_provided_rejection());
                }
                enforce_csrf(parts, state, location, &session).map(|()| CsrfGuard)
            }
            Err(err) if err.status_code == 500 => Err(ApiError::from(err).into()),
            Err(_) => Ok(CsrfGuard),
        }
    }
}

/// The session half shared by [`AuthenticatedSession`] and [`MfaSetupSession`] — everything
/// `ServeHTTP` does before `MfaRequired`.
async fn resolve_required_session(
    parts: &Parts,
    state: &AppState,
) -> Result<Session, SessionRejection> {
    let Some((token, location)) = parse_auth_token(parts) else {
        // Go's `ApiSessionRequired` with no token at all returns
        // `api.context.session_expired.app_error` rather than a "missing token" id.
        return Err(SessionRejection::no_token());
    };

    // Port of `handlers.go:273-280`. **`GetSession`'s error id does not reach the client.**
    // The web layer keeps a 500 as-is and replaces every other failure with the generic
    // `api.context.session_expired.app_error`, so a wrong token, an expired session, a
    // session revoked for idleness and a session id used as a token all produce one
    // indistinguishable 401 — which is the point: none of them tells a caller whether the
    // credential ever existed.
    //
    // This server used to return `api.context.invalid_token.error` here, which is the id
    // `App::GetSession` builds and Go then discards. Clients switch on the id, so that was a
    // wire divergence on every migrated route; it was found by the parity suite for the idle
    // timeout, which is the first test to compare a 401 body against Go's.
    //
    // `h.RequireSession` is true for every route that takes this extractor at all — a handler
    // that did not need a session would not name it.
    //
    // Not ported: `c.RemoveSessionCookie(w, r)`, which Go calls first. See [D-169].
    let session = state.app.get_session(&token).await.map_err(|err| {
        SessionRejection::for_get_session_error(err, || state.app.config().subpath())
    })?;
    // `checkCSRFToken` (handlers.go:295) — after the session resolves, before `SessionRequired`
    // and `MfaRequired`.
    enforce_csrf(parts, state, location, &session)?;
    Ok(session)
}

impl FromRequestParts<AppState> for AuthenticatedSession {
    type Rejection = SessionRejection;

    /// `APISessionRequired` and its `TrustRequester` and `DisableWhenBusy` variants — every
    /// handler Go registers with `RequireMfa: true` (api4/handlers.go:57, :165, :186).
    ///
    /// **`MfaRequired` runs last** (web/handlers.go:345), after the session is resolved. It does
    /// nothing unless the licence, `EnableMultifactorAuthentication` and
    /// `EnforceMultifactorAuthentication` all say so; then a user who has not set MFA up is
    /// refused at 403 on every route but `/api/v4/users/me` and the two that set it up
    /// ([`MfaSetupSession`]). The refusal clears no cookie: the session is valid.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = resolve_required_session(parts, state).await?;
        let is_users_me = is_users_me_path(parts.uri.path(), &state.app.config().subpath());
        state
            .app
            .mfa_required(Some(&session), is_users_me)
            .await
            .map_err(SessionRejection::mfa_required)?;
        Ok(AuthenticatedSession(session))
    }
}

/// `APISessionRequiredMfa` (api4/handlers.go:115) — identical to [`AuthenticatedSession`] except
/// `RequireMfa: false`. Taken only by `PUT /users/{user_id}/mfa` and
/// `POST /users/{user_id}/mfa/generate`: a user who owes MFA has to be able to set it up.
pub struct MfaSetupSession(pub Session);

impl FromRequestParts<AppState> for MfaSetupSession {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(MfaSetupSession(
            resolve_required_session(parts, state).await?,
        ))
    }
}

/// `rctx.Path() == path.Join(subpath, "/api/v4/users/me")` (app/authentication.go:411) — the
/// request's **path only**, query excluded, compared exactly: `/api/v4/users/me/` and
/// `/api/v4/users/me/teams` are not the exemption.
fn is_users_me_path(path: &str, subpath: &str) -> bool {
    path == mm_model::go_path::join(&[subpath, "/api/v4/users/me"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn only_the_exact_users_me_path_under_the_subpath_is_exempt() {
        assert!(is_users_me_path("/api/v4/users/me", "/"));
        assert!(is_users_me_path("/mm/api/v4/users/me", "/mm"));
        assert!(
            !is_users_me_path("/api/v4/users/me", "/mm"),
            "the subpath is part of it"
        );
        for other in [
            "/api/v4/users/me/",
            "/api/v4/users/me/teams",
            "/api/v4/users/ME",
            "/api/v4/users/abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(!is_users_me_path(other, "/"), "{other}");
        }
    }

    fn parts_with(headers: &[(&str, &str)], uri: &str) -> Parts {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("request builds").into_parts().0
    }

    /// The 500 arm: the store failed, so the error propagates unchanged and **no cookie is
    /// cleared**.
    ///
    /// Unreachable from the parity suite — it needs a broken database — and therefore the one
    /// branch here that only a unit test can hold down.
    #[test]
    fn a_store_failure_propagates_and_clears_nothing() {
        let inner = mm_model::utils::AppError::boxed(
            "GetSession",
            "app.session.get.app_error",
            None,
            String::new(),
            500,
        );
        let rejection = SessionRejection::for_get_session_error(inner, || {
            panic!("the subpath must not be consulted on the 500 path")
        });

        assert!(rejection.clear_session_cookie.is_none());
        assert_eq!(rejection.error.0.id, "app.session.get.app_error");
        assert_eq!(rejection.error.0.status_code, 500);
    }

    /// Every other status becomes the generic 401 **and** clears the cookie, scoped to the
    /// configured subpath.
    #[test]
    fn a_rejected_token_becomes_the_generic_401_and_clears_the_cookie() {
        let inner = mm_model::utils::AppError::boxed(
            "GetSession",
            "api.context.invalid_token.error",
            None,
            String::new(),
            401,
        );
        let rejection = SessionRejection::for_get_session_error(inner, || "/mattermost".to_owned());

        assert_eq!(
            rejection.clear_session_cookie.as_deref(),
            Some("/mattermost")
        );
        assert_eq!(
            rejection.error.0.id, "api.context.session_expired.app_error",
            "GetSession's own id is discarded by the web layer"
        );
        assert_eq!(rejection.error.0.status_code, 401);
    }

    const CSRF: &str = "csrfcsrfcsrfcsrfcsrfcsrf12";

    fn session_with_csrf(csrf: Option<&str>) -> Session {
        let mut session = Session::default();
        if let Some(csrf) = csrf {
            session.add_prop(mm_model::session::SESSION_PROP_CSRF, csrf);
        }
        session
    }

    fn request(method: Method, headers: &[(&str, &str)], trusted: bool) -> Parts {
        let mut builder = Request::builder().method(method).uri("/api/v4/users/ids");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut parts = builder.body(()).expect("request builds").into_parts().0;
        if trusted {
            parts.extensions.insert(TrustRequester);
        }
        parts
    }

    fn check(
        parts: &Parts,
        location: TokenLocation,
        csrf: Option<&str>,
        strict: bool,
    ) -> CsrfCheck {
        check_csrf_token(parts, location, &session_with_csrf(csrf), strict)
    }

    /// `tokenLocation == TokenLocationCookie && !h.TrustRequester && r.Method != "GET"` — each
    /// conjunct alone turns the check off, and a request that fails everything else is used so
    /// that "not checked" cannot be confused with "passed".
    #[test]
    fn the_check_runs_only_for_a_cookie_write_on_an_untrusted_route() {
        let bare = request(Method::POST, &[], false);
        assert_eq!(
            check(&bare, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Failed
        );
        for location in [TokenLocation::Header, TokenLocation::QueryString] {
            assert_eq!(
                check(&bare, location, Some(CSRF), false),
                CsrfCheck::NotNeeded,
                "{location:?}"
            );
        }
        let get = request(Method::GET, &[], false);
        assert_eq!(
            check(&get, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::NotNeeded
        );
        let trusted = request(Method::POST, &[], true);
        assert_eq!(
            check(&trusted, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::NotNeeded
        );
    }

    /// Only the literal `GET` is exempt: `HEAD`, `OPTIONS` and every write are checked.
    #[test]
    fn head_and_every_write_are_checked() {
        for method in [
            Method::HEAD,
            Method::OPTIONS,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            let parts = request(method.clone(), &[], false);
            assert_eq!(
                check(&parts, TokenLocation::Cookie, Some(CSRF), false),
                CsrfCheck::Failed,
                "{method}"
            );
        }
    }

    #[test]
    fn the_matching_token_passes_and_any_other_fails() {
        let right = request(Method::POST, &[(HEADER_CSRF_TOKEN, CSRF)], false);
        assert_eq!(
            check(&right, TokenLocation::Cookie, Some(CSRF), true),
            CsrfCheck::Passed
        );
        let wrong = request(Method::POST, &[(HEADER_CSRF_TOKEN, "nope")], false);
        assert_eq!(
            check(&wrong, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Failed
        );
        // Header names are case-insensitive, as `Header.Get` canonicalises them.
        let lower = request(Method::POST, &[("x-csrf-token", CSRF)], false);
        assert_eq!(
            check(&lower, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Passed
        );
        // The value is not.
        let upper = request(
            Method::POST,
            &[(HEADER_CSRF_TOKEN, "CSRFCSRFCSRFCSRFCSRFCSRF12")],
            false,
        );
        assert_eq!(
            check(&upper, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Failed
        );
    }

    /// `csrfHeader == session.GetCSRF()` with both empty is a pass: a session with no `csrf` prop
    /// accepts a cookie write with no header. Reproduced, not tightened.
    #[test]
    fn a_session_without_a_csrf_prop_passes_a_request_without_the_header() {
        let bare = request(Method::POST, &[], false);
        assert_eq!(
            check(&bare, TokenLocation::Cookie, None, true),
            CsrfCheck::Passed
        );
        let some = request(Method::POST, &[(HEADER_CSRF_TOKEN, CSRF)], false);
        assert_eq!(
            check(&some, TokenLocation::Cookie, None, true),
            CsrfCheck::Failed
        );
    }

    /// `X-Requested-With: XMLHttpRequest` rescues a mismatch only when enforcement is lenient,
    /// and only with that exact value.
    #[test]
    fn the_legacy_header_passes_only_when_not_strict() {
        let xhr = request(
            Method::POST,
            &[
                (HEADER_CSRF_TOKEN, "nope"),
                (HEADER_REQUESTED_WITH, "XMLHttpRequest"),
            ],
            false,
        );
        assert_eq!(
            check(&xhr, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Passed
        );
        assert_eq!(
            check(&xhr, TokenLocation::Cookie, Some(CSRF), true),
            CsrfCheck::Failed
        );
        let other = request(
            Method::POST,
            &[(HEADER_REQUESTED_WITH, "xmlhttprequest")],
            false,
        );
        assert_eq!(
            check(&other, TokenLocation::Cookie, Some(CSRF), false),
            CsrfCheck::Failed
        );
    }

    /// The CSRF refusal is the generic session 401 and clears the cookie.
    #[test]
    fn a_csrf_failure_is_the_session_401_and_clears_the_cookie() {
        let rejection = SessionRejection::csrf_failed("/".to_owned());
        assert_eq!(rejection.clear_session_cookie.as_deref(), Some("/"));
        assert_eq!(
            rejection.error.0.id,
            "api.context.session_expired.app_error"
        );
        assert_eq!(rejection.error.0.status_code, 401);
    }

    /// The cookie Go writes on a rejected token, byte for byte.
    #[test]
    fn the_removal_cookie_matches_gos_rendering() {
        assert_eq!(
            remove_session_cookie_header("/"),
            "MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly"
        );
        assert_eq!(
            remove_session_cookie_header("/mattermost"),
            "MMAUTHTOKEN=; Path=/mattermost; Max-Age=0; HttpOnly"
        );
    }

    /// An empty subpath — what a `SiteURL` Go cannot parse produces — omits the attribute
    /// entirely rather than sending `Path=`.
    #[test]
    fn an_empty_subpath_omits_the_path_attribute() {
        assert_eq!(
            remove_session_cookie_header(""),
            "MMAUTHTOKEN=; Max-Age=0; HttpOnly"
        );
    }

    /// `sanitizeCookiePath` keeps `0x20..0x7f` except `;`. A space is **valid** in a cookie path,
    /// which is worth pinning because it looks like it should not be — and a `SiteURL` of
    /// `https://host/spaced%20path` really does produce one.
    #[test]
    fn the_cookie_path_is_sanitized_like_go() {
        assert_eq!(sanitize_cookie_path("/spaced path"), "/spaced path");
        assert_eq!(sanitize_cookie_path("/a;b"), "/ab", "semicolons go");
        assert_eq!(sanitize_cookie_path("/a\u{7f}b"), "/ab", "DEL goes");
        assert_eq!(sanitize_cookie_path("/a\nb"), "/ab", "control bytes go");
        assert_eq!(sanitize_cookie_path("/caf\u{e9}"), "/caf", "and non-ASCII");
    }

    #[test]
    fn bearer_header_is_parsed() {
        let parts = parts_with(&[("Authorization", "Bearer abc123")], "/api/v4/users/me");
        assert_eq!(
            parse_auth_token(&parts),
            Some(("abc123".to_owned(), TokenLocation::Header))
        );
    }

    /// Go upper-cases the first six bytes before comparing, so the scheme is case-insensitive.
    #[test]
    fn bearer_header_is_case_insensitive() {
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let parts = parts_with(&[("Authorization", &format!("{scheme} abc123"))], "/");
            assert_eq!(
                parse_auth_token(&parts).map(|(t, _)| t),
                Some("abc123".to_owned()),
                "scheme {scheme} should parse"
            );
        }
    }

    #[test]
    fn token_scheme_is_parsed() {
        let parts = parts_with(&[("Authorization", "token abc123")], "/");
        assert_eq!(
            parse_auth_token(&parts),
            Some(("abc123".to_owned(), TokenLocation::Header))
        );
    }

    #[test]
    fn cookie_is_parsed() {
        let parts = parts_with(&[("Cookie", "MMAUTHTOKEN=cookievalue")], "/");
        assert_eq!(
            parse_auth_token(&parts),
            Some(("cookievalue".to_owned(), TokenLocation::Cookie))
        );
    }

    /// The precedence that would be easiest to get backwards, and the one with the worst
    /// consequence: the two servers would authenticate the same request as different users.
    #[test]
    fn cookie_beats_the_authorization_header() {
        let parts = parts_with(
            &[
                ("Authorization", "Bearer fromheader"),
                ("Cookie", "MMAUTHTOKEN=fromcookie"),
            ],
            "/",
        );
        assert_eq!(
            parse_auth_token(&parts),
            Some(("fromcookie".to_owned(), TokenLocation::Cookie))
        );
    }

    #[test]
    fn cookie_is_found_among_others() {
        let parts = parts_with(&[("Cookie", "foo=bar; MMAUTHTOKEN=tok; baz=qux")], "/");
        assert_eq!(
            parse_auth_token(&parts).map(|(t, _)| t),
            Some("tok".to_owned())
        );
    }

    #[test]
    fn query_string_is_the_last_resort() {
        let parts = parts_with(&[], "/api/v4/users/me?access_token=fromquery");
        assert_eq!(
            parse_auth_token(&parts),
            Some(("fromquery".to_owned(), TokenLocation::QueryString))
        );
    }

    #[test]
    fn an_empty_access_token_query_param_is_not_a_token() {
        let parts = parts_with(&[], "/api/v4/users/me?access_token=");
        assert_eq!(parse_auth_token(&parts), None);
    }

    #[test]
    fn no_credentials_at_all_yields_none() {
        let parts = parts_with(&[], "/api/v4/users/me");
        assert_eq!(parse_auth_token(&parts), None);
    }

    /// Go's deferred block truncates the returned value, not a log copy. Unreachable with real
    /// 26-character tokens, but it is behaviour and it is reproduced.
    #[test]
    fn a_token_longer_than_fifty_bytes_is_truncated() {
        let long = "a".repeat(80);
        let parts = parts_with(&[("Authorization", &format!("Bearer {long}"))], "/");
        let (token, _) = parse_auth_token(&parts).expect("parses");
        assert_eq!(token.len(), MAX_TOKEN_LEN);
        assert_eq!(token, "a".repeat(MAX_TOKEN_LEN));
    }

    /// The char-boundary handling in `parse_auth_token` is defensive, not reachable: a header
    /// value that is not visible ASCII fails `to_str`, so a multi-byte token never gets as far as
    /// the truncation. Asserted so the claim is measured rather than assumed — if a future change
    /// starts accepting non-ASCII header bytes, this test says so.
    #[test]
    fn a_non_ascii_authorization_header_yields_no_token_at_all() {
        let long = "é".repeat(40); // 80 bytes, 40 chars
        let parts = parts_with(&[("Authorization", &format!("Bearer {long}"))], "/");
        assert_eq!(
            parse_auth_token(&parts),
            None,
            "non-ASCII header values are not parseable as text, so no token is found"
        );
    }
}
