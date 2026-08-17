//! Port of `app.ParseAuthTokenFromRequest` (channels/app/authentication.go:493) and the session
//! half of `web.Context.ApiSessionRequired`.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
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

impl FromRequestParts<AppState> for AuthenticatedSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some((token, _location)) = parse_auth_token(parts) else {
            // Go's `ApiSessionRequired` with no token at all returns
            // `api.context.session_expired.app_error` rather than a "missing token" id.
            return Err(ApiError::unauthenticated());
        };

        let session = state
            .app
            .get_session(&token)
            .await
            .map_err(ApiError::from)?;
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
