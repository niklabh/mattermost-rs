//! Port of `app.ParseAuthTokenFromRequest` (channels/app/authentication.go:493) and the session
//! half of `web.Context.ApiSessionRequired`.

use axum::extract::FromRequestParts;
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

impl FromRequestParts<AppState> for AuthenticatedSession {
    type Rejection = SessionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some((token, _location)) = parse_auth_token(parts) else {
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
        Ok(AuthenticatedSession(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

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
