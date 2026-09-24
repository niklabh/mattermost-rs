//! The Strangler Fig proxy: everything not served here goes to the Go server, unaltered.
//!
//! This is the half of the design that lets the port ship before it is finished. A route that has
//! not been migrated is not a 404 — it is forwarded, and the client cannot tell the difference.
//! The correctness bar is therefore *transparency*: the client's request must reach Go as it was
//! sent, and Go's answer must reach the client as it was written.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
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
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Copy headers, dropping the ones that belong to a single connection.
///
/// **`Accept-Encoding` goes through.** Go's gzip wrapper answers it, and the forwarding client
/// decodes nothing (reqwest is built without its decompression features, and
/// [`crate::AppState::forward_http`] turns them off besides), so Go's `Content-Encoding` and
/// compressed bytes come back to the client exactly as Go wrote them. Dropping it — which this
/// did until D-208 — gave every forwarded route an uncompressed answer Go would have compressed.
/// A forwarded response is never compressed a second time: `go_global_headers` leaves anything
/// marked `x-mmrs-served-by: go` alone.
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
/// # A request that arrived on the local socket is forwarded over the local socket
///
/// The local router (`crate::local`) carries the Go server's socket path as a
/// [`crate::local::GoLocalSocket`] request extension, and the HTTP handlers it shares with the
/// TCP router forward through *this* function when they cannot answer. Dialling the port for
/// such a request would reach `APISessionRequired` instead of `APILocal` — a 401 where Go's
/// socket answers — so the extension, when present, redirects the leg to
/// [`crate::local::forward_over_unix`] before anything here runs. The check is on the request
/// and not on a parameter because the handlers that forward are written once for both routers;
/// the transport is a property of where the request came from, not of who is answering it.
///
/// # The call site travels with the response
///
/// `#[track_caller]` on a plain `fn` that returns the future, because the attribute is not
/// stable on an `async fn`. The caller's location is stamped on the response as a
/// [`ForwardSite`], which is how `crate::traffic` names *which* branch of a served handler handed
/// the request to Go. Through `Router::fallback` the location is inside axum, which is fine: that
/// request matched no route at all, and the log says so separately.
#[track_caller]
pub fn forward_to_go(
    State(state): State<AppState>,
    request: Request,
) -> impl std::future::Future<Output = Response> + Send + 'static {
    let site = ForwardSite(std::panic::Location::caller());
    async move {
        let mut response = forward(state, request).await;
        response.extensions_mut().insert(site);
        response
    }
}

/// The source line that called [`forward_to_go`], as a response extension. Read by
/// `crate::traffic`; nothing on the wire carries it.
#[derive(Clone, Copy, Debug)]
pub struct ForwardSite(pub &'static std::panic::Location<'static>);

/// # A `HEAD` answer keeps Go's `Content-Length`
///
/// Everywhere else the upstream `Content-Length` is dropped and hyper frames the body it is
/// handed, which is the same number. A `HEAD` answer has no body to frame: Go's header states
/// the length a `GET` would have had (`net/http` writes it when the handler set it or wrote
/// bytes), so it is carried verbatim, and hyper writes a user-set length on a `HEAD` as given.
/// When Go sent none — its handler wrote nothing (`chunkWriter.writeHeader`'s
/// `!isHEAD || len(p) > 0`) — [`crate::web_static::head_framing`] hides the empty body's size
/// so axum does not invent `Content-Length: 0` either. A `204` or `304` needs nothing: Go
/// suppresses the header on both (`suppressedHeaders`), and hyper writes none for an empty body
/// there.
#[tracing::instrument(skip_all, fields(method = %request.method(), path = request.uri().path(), upstream_status))]
async fn forward(state: AppState, request: Request) -> Response {
    if let Some(go) = request.extensions().get::<crate::local::GoLocalSocket>() {
        // Cloned because the path is borrowed from the request that is about to be moved.
        let socket = std::sync::Arc::clone(&go.0);
        return crate::local::forward_over_unix(&socket, request).await;
    }

    let (parts, body) = request.into_parts();
    let head = parts.method == Method::HEAD;

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
        .forward_http
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
            if !is_hop_by_hop(name.as_str()) || (head && name == header::CONTENT_LENGTH) {
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
            .map(|response| crate::web_static::head_framing(&parts.method, response))
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

    /// A canned Go: each request line maps to the exact bytes `net/http` would write for it, so
    /// the forward leg is exercised through a real socket on both sides — hyper's framing is the
    /// subject, and only the wire shows it.
    async fn canned_upstream() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let line = text.lines().next().unwrap_or_default();
                    let answer: &str = match line {
                        "HEAD /sized HTTP/1.1" => {
                            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 248\r\nConnection: close\r\n\r\n"
                        }
                        "HEAD /unsized HTTP/1.1" => {
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
                        }
                        "GET /sized HTTP/1.1" => {
                            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                        }
                        "GET /empty HTTP/1.1" => {
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        }
                        "GET /not-modified HTTP/1.1" | "HEAD /not-modified HTTP/1.1" => {
                            "HTTP/1.1 304 Not Modified\r\nEtag: \"x\"\r\nConnection: close\r\n\r\n"
                        }
                        "GET /no-content HTTP/1.1" | "HEAD /no-content HTTP/1.1" => {
                            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
                        }
                        _ => {
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        }
                    };
                    let _ = stream.write_all(answer.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The proxy mounted as the fallback of an otherwise empty router, as in production.
    async fn proxy_in_front_of(upstream: String) -> String {
        let app = mm_app::App::new(mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://x/y")
                .unwrap(),
        ));
        let router = axum::Router::new()
            .fallback(forward_to_go)
            .with_state(AppState::new(app, upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr.to_string()
    }

    /// The raw response head, lower-cased, and the body bytes.
    async fn raw(addr: &str, method: &str, path: &str) -> (Vec<String>, Vec<u8>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&bytes[..split])
            .lines()
            .map(str::to_ascii_lowercase)
            .collect();
        (head, bytes[split + 4..].to_vec())
    }

    fn framing(head: &[String]) -> Vec<&str> {
        head.iter()
            .map(String::as_str)
            .filter(|l| l.starts_with("content-length:") || l.starts_with("transfer-encoding:"))
            .collect()
    }

    /// D-903: a forwarded `HEAD` states Go's length, and states none when Go stated none. The
    /// `GET`, `204` and `304` rows pin that nothing else moved with it.
    #[tokio::test]
    async fn a_forwarded_answer_is_framed_as_go_framed_it() {
        let proxy = proxy_in_front_of(canned_upstream().await).await;
        // (method, path, status, framing headers, body)
        type Case<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [u8]);
        let cases: [Case; 8] = [
            ("HEAD", "/sized", "404", &["content-length: 248"], b""),
            ("HEAD", "/unsized", "200", &[], b""),
            ("GET", "/sized", "200", &["content-length: 5"], b"hello"),
            ("GET", "/empty", "200", &["content-length: 0"], b""),
            ("GET", "/not-modified", "304", &[], b""),
            ("HEAD", "/not-modified", "304", &[], b""),
            ("GET", "/no-content", "204", &[], b""),
            ("HEAD", "/no-content", "204", &[], b""),
        ];
        for (method, path, status, want, body) in cases {
            let (head, got_body) = raw(&proxy, method, path).await;
            assert!(
                head[0].starts_with(&format!("http/1.1 {status}")),
                "{method} {path}: {head:?}"
            );
            assert_eq!(framing(&head), want, "{method} {path}: {head:?}");
            assert_eq!(got_body, body, "{method} {path}");
        }
    }
}
