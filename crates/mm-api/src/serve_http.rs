//! The first steps of `web.Handler.ServeHTTP` (web/handlers.go:151) that come before any handler
//! code, for the routes this server serves: `basicSecurityChecks` and the per-user rate limit.
//!
//! Applied as a `route_layer`, so it wraps every served route and none of the fallbacks: a request
//! no route claims is either answered by `web_static::fallback` (which runs the same two steps for
//! the web client's handlers) or forwarded, and Go runs them itself.
//!
//! # Gorilla first
//!
//! A path segment outside gorilla's class for its parameter (`crate::segment_matches_go_mux_for`)
//! never reaches a `web.Handler` in Go: it falls to the api4 catch-all, a bare `HandlerFunc` with
//! neither step. Such a request is forwarded by `mux_segments_or_forward`, which runs *inside*
//! this layer, so the same question is asked here first and the request let through untouched.

use axum::extract::{RawPathParams, Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::ApiError;

/// Whether gorilla would have matched every path parameter of the route axum chose.
fn gorilla_matched(params: &RawPathParams) -> bool {
    params
        .iter()
        .all(|(name, value)| crate::segment_matches_go_mux_for(name, value))
}

/// Port of `basicSecurityChecks` (web/handlers.go:143): `len(r.RequestURI)` against
/// `ServiceSettings.MaximumURLLength`, strictly greater. `RequestURI` is the request target as
/// sent — path and query for the origin form every client uses, the whole URL for the absolute
/// form — which is what the URI's display writes back.
fn uri_too_long(request: &Request, max: i64) -> bool {
    let length = request.uri().to_string().len();
    i64::try_from(length).unwrap_or(i64::MAX) > max
}

/// The refusal: `c.Err` set before `ServeHTTP` has written a single header, so
/// `handleContextError` writes the JSON error on a bare response — `Content-Type` and, from the
/// `gzhttp` wrapper around every API handler in `gzip` mode, `Vary`. Not the security headers,
/// not `X-Request-Id`: both come later in `ServeHTTP`. The body's `request_id` is the one it
/// minted. Measured: `GET /api/v4/system/ping?x=<3000 bytes>` on Go.
fn url_too_long(gzip: bool) -> Response {
    let mut response = ApiError::from(mm_model::utils::AppError::new(
        "basicSecurityChecks",
        "basic_security_check.url.too_long_error",
        None,
        "",
        414,
    ))
    .into_response();
    if gzip {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    // `go_global_headers` would add the security set, which Go has not written yet.
    response
        .extensions_mut()
        .insert(crate::web_static::WebOwnHeaders);
    response
}

/// The preamble on the TCP router: `basicSecurityChecks`, then `UserIdRateLimit`
/// (web/handlers.go:288). See [`crate::ratelimit`] for the second.
pub(crate) async fn preamble(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Response {
    if !gorilla_matched(&params) {
        return next.run(request).await;
    }
    if uri_too_long(&request, state.app.config().maximum_url_length) {
        return url_too_long(state.api_gzip);
    }
    let (parts, body) = request.into_parts();
    let Some(verdict) = crate::ratelimit::per_user_verdict(&state, &parts).await else {
        return next.run(Request::from_parts(parts, body)).await;
    };
    if verdict.limited {
        return crate::ratelimit::per_user_refusal(&verdict);
    }
    let mut response = next.run(Request::from_parts(parts, body)).await;
    crate::ratelimit::append_verdict(&mut response, &verdict);
    response
}

/// The preamble on the local-mode router: `basicSecurityChecks` only. The socket's requests carry
/// no token, and the local server is not wrapped in the global limiter.
pub(crate) async fn local_preamble(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Response {
    if gorilla_matched(&params) && uri_too_long(&request, state.app.config().maximum_url_length) {
        return url_too_long(state.api_gzip);
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: &str) -> Request {
        axum::http::Request::builder()
            .uri(target)
            .body(axum::body::Body::empty())
            .expect("a request")
    }

    /// Strictly greater: a target of exactly the limit passes, one byte more does not; the query
    /// counts, as it is part of `RequestURI`.
    #[test]
    fn the_limit_is_on_the_whole_request_target_and_strictly_greater() {
        let at = format!("/api/v4/x?q={}", "a".repeat(2048 - 12));
        assert_eq!(at.len(), 2048);
        assert!(!uri_too_long(&request(&at), 2048));
        assert!(uri_too_long(&request(&format!("{at}a")), 2048));
        assert!(uri_too_long(&request("/api/v4/users"), 12));
        assert!(!uri_too_long(&request("/api/v4/users"), 13));
        assert!(
            uri_too_long(&request("http://h/api/v4/users"), 13),
            "the absolute form counts whole"
        );
    }

    #[test]
    fn the_refusal_is_bare_json_with_the_gzip_vary_only_in_gzip_mode() {
        let response = url_too_long(true);
        assert_eq!(response.status(), 414);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["vary"], "Accept-Encoding");
        assert!(
            response
                .extensions()
                .get::<crate::web_static::WebOwnHeaders>()
                .is_some()
        );
        assert!(!url_too_long(false).headers().contains_key("vary"));
    }
}
