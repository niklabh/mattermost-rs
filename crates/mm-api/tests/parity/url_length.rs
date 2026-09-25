//! Cross-server parity for **`basicSecurityChecks`** (web/handlers.go:143): a request URI longer
//! than `ServiceSettings.MaximumURLLength` (2048 on the stack) is the 414
//! `basic_security_check.url.too_long_error` from every `web.Handler`, before any other step —
//! on the API router and the local socket alike. Reproduced by `mm_api::serve_http` (D-1211).
//!
//! ```sh
//! scripts/parity.sh --test parity url_length
//! ```

use super::web_client::{Raw, raw_request};
use crate::common::local_socket::{go_socket, over_socket, rust_socket, sockets_enabled};
use crate::common::{self, GO, RUST, stack_enabled};

/// `path?q=aaa…` of exactly `length` bytes.
fn target_of(path: &str, length: usize) -> String {
    let prefix = format!("{path}?q=");
    format!("{prefix}{}", "a".repeat(length - prefix.len()))
}

/// The body with its per-request id removed, when it is JSON.
fn without_request_id(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.remove("request_id");
    }
    value
}

fn assert_same_414(go: &Raw, rust: &Raw, context: &str) {
    assert_eq!(go.status, 414, "{context}: Go");
    assert_eq!(rust.status, 414, "{context}");
    assert_eq!(go.headers, rust.headers, "{context}: headers");
    assert_eq!(
        without_request_id(&go.body),
        without_request_id(&rust.body),
        "{context}: body"
    );
    assert_eq!(rust.served_by.as_deref(), Some("rust"), "{context}");
}

/// At, just under and just over the limit, on routes with and without a session, a write, and a
/// translated error: only the one byte over is refused, and the refusal is Go's byte for byte
/// (its request id aside).
#[tokio::test]
async fn one_byte_over_the_limit_is_gos_414() {
    if !stack_enabled() {
        return;
    }
    let http = common::client();
    let token = common::go_minted_token(&http).await;
    let auth = format!("Bearer {token}");
    for (method, path, headers) in [
        ("GET", "/api/v4/system/ping", vec![]),
        (
            "GET",
            "/api/v4/users/me",
            vec![("Authorization", auth.as_str())],
        ),
        ("GET", "/api/v4/users/me", vec![]),
        ("POST", "/api/v4/users/login", vec![("Content-Length", "0")]),
        (
            "GET",
            "/api/v4/system/ping",
            vec![("Accept-Language", "de")],
        ),
        (
            "GET",
            "/api/v4/system/ping",
            vec![("Accept-Encoding", "gzip")],
        ),
    ] {
        for length in [2047, 2048] {
            let target = target_of(path, length);
            let (go, rust) = (
                raw_request(GO, method, &target, &headers).await,
                raw_request(RUST, method, &target, &headers).await,
            );
            assert_ne!(go.status, 414, "{method} {path} at {length}");
            assert_eq!(go.status, rust.status, "{method} {path} at {length}");
        }
        let target = target_of(path, 2049);
        let go = raw_request(GO, method, &target, &headers).await;
        let rust = raw_request(RUST, method, &target, &headers).await;
        assert_same_414(&go, &rust, &format!("{method} {path} {headers:?} at 2049"));
    }
}

/// A segment gorilla's class refuses is the api4 catch-all's 404 however long the URI is: the
/// catch-all is not a `web.Handler`, so it never checks.
#[tokio::test]
async fn a_path_gorilla_does_not_route_is_its_404_at_any_length() {
    if !stack_enabled() {
        return;
    }
    let target = target_of("/api/v4/users/not-an-id", 3000);
    let go = raw_request(GO, "GET", &target, &[]).await;
    let rust = raw_request(RUST, "GET", &target, &[]).await;
    assert_eq!(go.status, 404);
    assert_eq!(rust.status, 404);
}

/// The local socket's `APILocal` handlers are `web.Handler`s too.
#[tokio::test]
async fn the_local_socket_refuses_the_same_uri() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");
    let per_process = ["date", "x-mmrs-served-by"];
    for (length, want) in [(2048, 200), (2049, 414)] {
        let target = target_of("/api/v4/system/ping", length);
        let (go_status, go_headers, go_body) = over_socket(&go, "GET", &target).await;
        let (rust_status, rust_headers, rust_body) = over_socket(&rust, "GET", &target).await;
        assert_eq!(go_status, want, "Go at {length}");
        assert_eq!(rust_status, want, "at {length}");
        if want == 414 {
            let strip = |h: &axum::http::HeaderMap| {
                let mut v: Vec<(String, String)> = h
                    .iter()
                    .filter(|(k, _)| !per_process.contains(&k.as_str()))
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
                    .collect();
                v.sort();
                v
            };
            assert_eq!(strip(&go_headers), strip(&rust_headers));
            assert_eq!(without_request_id(&go_body), without_request_id(&rust_body));
            assert_eq!(
                rust_headers.get("x-mmrs-served-by").map(|v| v.as_bytes()),
                Some(b"rust".as_slice())
            );
        }
    }
}
