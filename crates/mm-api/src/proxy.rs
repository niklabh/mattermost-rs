//! The Strangler Fig proxy: everything not served here goes to the Go server, unaltered.
//!
//! This is the half of the design that lets the port ship before it is finished. A route that has
//! not been migrated is not a 404 — it is forwarded, and the client cannot tell the difference.
//! The correctness bar is therefore *transparency*: the client's request must reach Go as it was
//! sent, and Go's answer must reach the client as it was written.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// Hop-by-hop headers (RFC 9110 §7.6.1). These describe a single connection, not the message, so
/// forwarding them corrupts the next hop's framing — `Connection: close` on the forward leg would
/// close the wrong socket, and a stale `Content-Length` contradicts the body actually written.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    // Not hop-by-hop in the RFC, but the client library sets it from the body it actually sends.
    // Carrying the inbound value risks contradicting that.
    "content-length",
    // reqwest applies its own decompression policy; a copied value can describe a body that has
    // already been decoded.
    "accept-encoding",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Copy headers, dropping the ones that belong to a single connection.
fn forwardable(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !is_hop_by_hop(name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Forward a request to the Go server and return its response verbatim.
///
/// Mounted as the router's fallback, so it catches every path no handler claimed. Adding a
/// migrated route is therefore purely additive: nothing here needs an entry removed from a list,
/// and there is no list to forget to update.
#[tracing::instrument(skip_all, fields(method = %request.method(), path = request.uri().path(), upstream_status))]
pub async fn forward_to_go(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();

    // The path and query go through untouched. Reconstructing them from parsed components would
    // risk normalising away an encoding the Go server is sensitive to.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", state.go_upstream, path_and_query);

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the client's request body");
            return (StatusCode::BAD_REQUEST, "could not read request body").into_response();
        }
    };

    let upstream = state
        .http
        .request(parts.method.clone(), &url)
        .headers(forwardable(&parts.headers))
        .body(body_bytes)
        .send()
        .await;

    let upstream = match upstream {
        Ok(response) => response,
        Err(err) => {
            // The Go server being unreachable is the migration's most consequential failure: every
            // unmigrated route is down. It is a 502 and it is logged at error, not warn.
            tracing::error!(error = %err, url = %url, "forward to the Go server failed");
            return (
                StatusCode::BAD_GATEWAY,
                "upstream Mattermost server is unreachable",
            )
                .into_response();
        }
    };

    let status = upstream.status();
    tracing::Span::current().record("upstream_status", status.as_u16());

    let mut response = Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        for (name, value) in upstream.headers() {
            if !is_hop_by_hop(name.as_str()) {
                headers.append(name.clone(), value.clone());
            }
        }
        // Announce which server answered. The client ignores it; an operator watching the cutover
        // does not, and without it a migrated route and a proxied one are indistinguishable.
        headers.insert("x-mmrs-served-by", HeaderValue::from_static("go"));
    }

    match upstream.bytes().await {
        Ok(bytes) => response
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
        Err(err) => {
            tracing::error!(error = %err, "could not read the Go server's response body");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_recognised_case_insensitively() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(is_hop_by_hop("content-length"));
        assert!(!is_hop_by_hop("Authorization"));
        assert!(!is_hop_by_hop("Cookie"));
        assert!(!is_hop_by_hop("X-Requested-With"));
    }

    /// The credentials must survive the hop or every proxied route is anonymous.
    #[test]
    fn credentials_and_content_type_are_forwarded() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer abc"));
        headers.insert("cookie", HeaderValue::from_static("MMAUTHTOKEN=abc"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("content-length", HeaderValue::from_static("12"));

        let out = forwardable(&headers);
        assert_eq!(
            out.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer abc")
        );
        assert!(out.contains_key("cookie"));
        assert!(out.contains_key("content-type"));
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("content-length"));
    }

    /// A repeated header must stay repeated — `Set-Cookie` is the one that matters, and
    /// `insert` would silently keep only the last.
    #[test]
    fn repeated_headers_are_preserved() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));

        let out = forwardable(&headers);
        let cookies: Vec<_> = out.get_all("set-cookie").iter().collect();
        assert_eq!(cookies.len(), 2);
    }
}
