//! Helpers for the **local-mode** parity suites: the same route over two unix sockets.
//!
//! Lifted out of `parity/local_mode.rs` on 2026-09-14 so that every `*_local.go` family can
//! have its own suite file without four copies of the socket client. See that module's docs for
//! the authentication model under test; the short version is that a request over the socket
//! carries no credential and gets `model.Session{Local: true}`, which is unrestricted.
//!
//! The client is `mm_api::local::send_over_unix` — mm-api's own forward leg to Go, used as a
//! test client so that a bug in it is visible here rather than hidden by a second transport.

use super::stack_enabled;

/// The Go server's local socket, or `None` when the stack has not exported one.
pub fn go_socket() -> Option<std::path::PathBuf> {
    std::env::var("MMRS_GO_LOCAL_SOCKET").ok().map(Into::into)
}

/// mm-api's local socket.
pub fn rust_socket() -> Option<std::path::PathBuf> {
    std::env::var("MMRS_LOCAL_SOCKET").ok().map(Into::into)
}

/// True when both sockets are named *and* present.
///
/// Named-but-absent is the ordinary state of a checkout whose `mm-api` predates this work, and it
/// must skip rather than fail — but the skip is logged, because a silently skipped parity test is
/// the failure mode `stack_enabled` exists to avoid.
pub fn sockets_enabled() -> bool {
    if !stack_enabled() {
        return false;
    }
    match (go_socket(), rust_socket()) {
        (Some(go), Some(rust)) if go.exists() && rust.exists() => true,
        (Some(go), Some(rust)) => {
            eprintln!(
                "skipping: local sockets not both present ({} exists: {}, {} exists: {})",
                go.display(),
                go.exists(),
                rust.display(),
                rust.exists()
            );
            false
        }
        _ => {
            eprintln!("skipping: MMRS_GO_LOCAL_SOCKET / MMRS_LOCAL_SOCKET are not set");
            false
        }
    }
}

/// One request over one socket, with a JSON body.
pub async fn post_over_socket(
    socket: &std::path::Path,
    path: &str,
    body: &'static str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body))
        .expect("request builds");

    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("POST {path} over {}: {e}", socket.display()));

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// One request over one socket, returning `(status, headers, body)`.
pub async fn over_socket(
    socket: &std::path::Path,
    method: &str,
    path: &str,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        // `mmctl` sends a Host; curl's `--unix-socket` sends whatever the URL says. Neither server
        // routes on it, and sending one keeps this an ordinary HTTP/1.1 request.
        .header("Host", "localhost")
        .body(axum::body::Body::empty())
        .expect("request builds");

    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{} {path} over {}: {e}", method, socket.display()));

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// The same request to both sockets.
pub async fn both(method: &str, path: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");

    let (go_status, _, go_body) = over_socket(&go, method, path).await;
    let (rust_status, rust_headers, rust_body) = over_socket(&rust, method, path).await;

    assert_eq!(
        rust_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{method} {path} was forwarded to Go over the socket, so this comparison proves nothing \
         about the Rust handler. Is an older mm-api still holding the socket?"
    );

    ((go_status, go_body), (rust_status, rust_body))
}

/// [`both`] without the "we served it" assertion, for a request that is *expected* to be
/// forwarded — a path segment outside Go's mux charset, or a pair nobody has migrated.
///
/// Returns each side's `(status, body)` plus whether ours carried `x-mmrs-served-by: rust`, so a
/// caller can assert the forward rather than merely tolerate it.
pub async fn both_maybe_forwarded(
    method: &str,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>), bool) {
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");

    let (go_status, _, go_body) = over_socket(&go, method, path).await;
    let (rust_status, rust_headers, rust_body) = over_socket(&rust, method, path).await;
    let served_here = rust_headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");

    ((go_status, go_body), (rust_status, rust_body), served_here)
}

/// One request with a body to both sockets, for any method.
pub async fn both_with_body(
    method: &str,
    path: &str,
    body: &'static str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let send = async |socket: &std::path::Path| {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len().to_string())
            .body(axum::body::Body::from(body.to_owned()))
            .expect("request builds");
        let response = mm_api::local::send_over_unix(socket, request)
            .await
            .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec();
        (status, headers, body)
    };

    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");
    let (go_status, _, go_body) = send(&go).await;
    let (rust_status, rust_headers, rust_body) = send(&rust).await;
    assert_eq!(
        rust_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{method} {path} was forwarded to Go over the socket"
    );
    ((go_status, go_body), (rust_status, rust_body))
}

/// Assert a **forwarded** body is Go's own, byte for byte apart from the per-request id.
///
/// [`super::assert_error_bodies_match_except_known_gaps`] is the wrong instrument here: it pins
/// our `message` to the raw error id, which is the D-092 i18n gap in errors *this server renders*.
/// A forwarded response was rendered by Go, so it carries Go's translated message — and a helper
/// that tolerated both would no longer be able to tell a forward from a local answer.
///
/// `request_id` is minted per request and the two calls are two requests, so it is dropped rather
/// than compared.
pub fn assert_forwarded_body_is_gos(go_body: &[u8], rust_body: &[u8], context: &str) {
    let strip = |body: &[u8]| {
        let mut value: serde_json::Value = serde_json::from_slice(body)
            .unwrap_or_else(|e| panic!("{context}: body is not JSON: {e}"));
        if let Some(object) = value.as_object_mut() {
            object.remove("request_id");
        }
        value
    };
    assert_eq!(
        strip(go_body),
        strip(rust_body),
        "{context}: a forwarded body must be Go's, message included"
    );
}
