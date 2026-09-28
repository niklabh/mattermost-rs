//! Cross-server parity for **`HEAD` on the api4 tree** (D-1110): gorilla registers no `HEAD`
//! on an api4 route except the three file reads, so every other `HEAD /api/v4/…` is the
//! catch-all's `Handle404`. Reproduced by `mm_api::mux_guard`, which also makes gorilla's clean-path redirect.
//!
//! ```sh
//! scripts/parity.sh --test parity api_head
//! ```
//!
//! Raw sockets, for the reason `web_client` gives: the `Content-Length` of a `HEAD` answer and
//! whether it has one at all are what is compared, and reqwest would hide both.

use super::web_client::{Raw, assert_equivalent, raw_request};
use crate::common::local_socket::{go_socket, over_socket, rust_socket, sockets_enabled};
use crate::common::{self, GO, RUST, stack_enabled};

/// Every api4 `GET` Go registers, as `(path template, served here, router)`, from
/// `scripts/routes.py` — the inventory the denominator is counted from, so a route added to
/// either side is swept without editing this file.
fn api4_gets() -> Vec<(String, bool, String)> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/routes.py");
    let out = std::process::Command::new("python3")
        .arg(script)
        .arg("--tsv")
        .output()
        .expect("python3 runs scripts/routes.py");
    assert!(out.status.success(), "routes.py --tsv failed");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            // `/manualtest` is in the inventory (api4/api.go:414) but outside the api4 tree.
            (cols.len() >= 5 && cols[1] == "GET" && cols[2].starts_with("/api/v4/"))
                .then(|| (cols[2].to_owned(), cols[0] == "SERVED", cols[4].to_owned()))
        })
        .collect()
}

/// A template with every `{param}` filled. The value is irrelevant to gorilla's `HEAD` answer —
/// a segment outside a parameter's class is the same catch-all — but a lower-case 26-letter id
/// is inside every class api4 uses, so the three file reads reach their handlers.
fn fill(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}').map_or(rest.len(), |c| open + c + 1);
        out.push_str("abcdefghijklmnopqrstuvwxyz");
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

fn is_file_read(path: &str) -> bool {
    path.starts_with("/api/v4/files/")
        && (path.matches('/').count() == 4
            || path.ends_with("/thumbnail")
            || path.ends_with("/preview"))
}

async fn both(method: &str, target: &str, headers: &[(&str, &str)]) -> (Raw, Raw) {
    (
        raw_request(GO, method, target, headers).await,
        raw_request(RUST, method, target, headers).await,
    )
}

/// The whole api4 `GET` inventory, served and forwarded, with and without a session: Go's `404`
/// with its length and nothing else, answered here. The three file reads, which do take `HEAD`,
/// are compared too and must not be 404s.
#[tokio::test]
async fn every_api4_get_answers_head_as_gorilla_does() {
    if !stack_enabled() {
        return;
    }
    let http = common::client();
    let token = common::go_minted_token(&http).await;
    let auth = format!("Bearer {token}");

    let routes = api4_gets();
    let http_routes: Vec<_> = routes.iter().filter(|(_, _, r)| r == "http").collect();
    assert!(
        http_routes.len() >= 235,
        "the inventory is all there ({})",
        http_routes.len()
    );

    let mut not_found = 0;
    let mut file_reads = 0;
    for (template, _, _) in http_routes {
        let target = fill(template);
        for headers in [vec![], vec![("Authorization", auth.as_str())]] {
            let (mut go, mut rust) = both("HEAD", &target, &headers).await;
            // `X-Version-Id` is not minted on a served API answer (D-207's closing note); only
            // the file reads, which reach their handlers, have one to drop.
            go.headers.retain(|(k, _)| k != "x-version-id");
            rust.headers.retain(|(k, _)| k != "x-version-id");
            assert_equivalent(&go, &rust, "HEAD", &target, &headers);
            if is_file_read(&target) {
                // The handler ran (its own 400 or not-found), not the catch-all: `ServeHTTP`'s
                // security headers are the difference.
                assert!(
                    go.header("referrer-policy").is_some(),
                    "HEAD {target} is registered in Go"
                );
                file_reads += 1;
                continue;
            }
            assert_eq!(go.status, 404, "HEAD {target} {headers:?}");
            assert_eq!(
                go.header("referrer-policy"),
                None,
                "the catch-all, not a handler"
            );
            assert_eq!(
                rust.served_by.as_deref(),
                Some("rust"),
                "HEAD {target} is answered here, not forwarded"
            );
            assert!(
                go.header("content-length").is_some_and(|l| l != "0"),
                "HEAD {target}: Go states the body's length"
            );
            not_found += 1;
        }
    }
    assert!(not_found >= 470, "{not_found} 404s compared");
    assert_eq!(file_reads, 6, "the three file reads, twice each");
}

/// The paths around the api4 tree that do take `HEAD`, or that gorilla answers before matching:
/// each must still be the answer it was. Compared whole; who answers is not asserted, since some
/// are forwarded by design.
#[tokio::test]
async fn head_outside_the_api4_catch_all_is_unchanged() {
    if !stack_enabled() {
        return;
    }
    for target in [
        "/api/v4",
        "/api/v5/x",
        "/robots.txt",
        "/static/no-such-asset.js",
        "/plugins/com.example.none/x",
        // gorilla's clean-path redirect runs before any route is matched.
        "/api/v4//system/ping",
        "/api/v4/system/./ping",
    ] {
        let (go, rust) = both("HEAD", target, &[]).await;
        assert_equivalent(&go, &rust, "HEAD", target, &[]);
    }
    // `getPublicFile` takes `HEAD` too; its refusal page carries a fresh ECDSA signature, so only
    // the status is comparable.
    let (go, rust) = both("HEAD", "/files/abcdefghijklmnopqrstuvwxyz/public", &[]).await;
    assert_eq!(
        (go.status, rust.status),
        (403, 403),
        "the public-link refusal"
    );
    for target in ["/api/v4//system/ping", "/api/v4/system/./ping"] {
        let (go, rust) = both("HEAD", target, &[]).await;
        assert_eq!(
            go.status, 301,
            "{target}: the redirect case is the redirect"
        );
        assert_eq!(rust.served_by.as_deref(), Some("rust"), "{target}");
    }
}

/// The body quotes the **decoded** path, which is what gorilla matched: an escaped space and an
/// escaped slash in the prefix change the length Go states, and so must change ours.
#[tokio::test]
async fn the_404_quotes_the_decoded_path() {
    if !stack_enabled() {
        return;
    }
    for target in [
        "/api/v4/users/a%20b",
        "/api%2Fv4/system/ping",
        "/api/v4/users/me?page=1&per_page=%20",
        "/api/v4/users/%3Cscript%3E",
    ] {
        let (go, rust) = both("HEAD", target, &[]).await;
        assert_eq!(go.status, 404, "{target}");
        assert_equivalent(&go, &rust, "HEAD", target, &[]);
        assert_eq!(rust.served_by.as_deref(), Some("rust"), "{target}");
    }
}

/// A body past `net/http`'s 2048-byte buffer is flushed before the handler returns, and a `HEAD`
/// that flushed gets neither a length nor chunks. Both sides of the threshold.
#[tokio::test]
async fn a_404_past_the_buffer_has_no_length() {
    if !stack_enabled() {
        return;
    }
    let mut lengths = Vec::new();
    // The body is 237 bytes plus the path, and the path is 8 plus the pad: 1803 is exactly 2048.
    for pad in [1800, 1803, 1804, 3000] {
        let target = format!("/api/v4/{}", "x".repeat(pad));
        let (go, rust) = both("HEAD", &target, &[]).await;
        assert_eq!(go.status, 404);
        assert_equivalent(&go, &rust, "HEAD", &target, &[]);
        assert_eq!(rust.served_by.as_deref(), Some("rust"));
        lengths.push(go.header("content-length").map(str::to_owned));
    }
    assert_eq!(
        lengths,
        [Some("2045".to_owned()), Some("2048".to_owned()), None, None]
    );
}

/// A method a partially migrated path does not serve is forwarded, and Go's answer comes back
/// without the `Allow` header axum's method router used to add to it. `HEAD` on a `POST`-only
/// path is the case D-1110 found; `DELETE` on a `GET`-only one is the same mechanism.
#[tokio::test]
async fn no_answer_carries_an_allow_header() {
    if !stack_enabled() {
        return;
    }
    for (method, target) in [
        ("HEAD", "/api/v4/users/login"),
        ("DELETE", "/api/v4/system/ping"),
        ("PUT", "/api/v4/users/login"),
    ] {
        let (go, rust) = both(method, target, &[]).await;
        assert_eq!(go.header("allow"), None, "{method} {target}: Go");
        assert_eq!(rust.header("allow"), None, "{method} {target}");
        assert_eq!(go.status, rust.status, "{method} {target}");
    }
}

/// The local-mode router registers no file route, so every api4 `HEAD` over the socket is the
/// catch-all (api4/api.go:527) — including the routes it serves.
#[tokio::test]
async fn every_local_get_answers_head_as_gorilla_does() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");
    let locals: Vec<_> = api4_gets()
        .into_iter()
        .filter(|(_, _, router)| router == "local")
        .collect();
    assert!(locals.len() >= 60, "{} local GETs", locals.len());
    let per_process = ["date", "x-request-id", "x-version-id", "x-mmrs-served-by"];
    for (template, _, _) in locals {
        let path = fill(&template);
        let (go_status, go_headers, _) = over_socket(&go, "HEAD", &path).await;
        let (rust_status, rust_headers, _) = over_socket(&rust, "HEAD", &path).await;
        assert_eq!(go_status, 404, "HEAD {path} over Go's socket");
        assert_eq!(rust_status, go_status, "HEAD {path}");
        let strip = |h: &axum::http::HeaderMap| {
            let mut v: Vec<(String, String)> = h
                .iter()
                .filter(|(k, _)| !per_process.contains(&k.as_str()))
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(strip(&go_headers), strip(&rust_headers), "HEAD {path}");
        assert_eq!(
            rust_headers
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "HEAD {path} is answered here"
        );
    }
}

/// The socket's `partially_migrated` forwards the same way, and adds no `Allow` either.
#[tokio::test]
async fn no_local_answer_carries_an_allow_header() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("present");
    let rust = rust_socket().expect("present");
    for (method, path) in [("DELETE", "/api/v4/system/ping"), ("PUT", "/api/v4/roles")] {
        let (go_status, go_headers, _) = over_socket(&go, method, path).await;
        let (rust_status, rust_headers, _) = over_socket(&rust, method, path).await;
        assert_eq!(go_status, rust_status, "{method} {path}");
        assert!(!go_headers.contains_key("allow"), "{method} {path}: Go");
        assert!(!rust_headers.contains_key("allow"), "{method} {path}");
    }
}

/// gorilla redirects an unclean path to its clean form before matching, on every method. A path an
/// axum route claims (`{user_id}` = `..`) used to reach the handler or be forwarded — and the
/// forward leg resolves dot segments, so Go answered the clean path's 404.
#[tokio::test]
async fn an_unclean_path_is_gorillas_redirect_on_every_method() {
    if !stack_enabled() {
        return;
    }
    for method in ["GET", "POST", "DELETE", "HEAD"] {
        for target in [
            "/api/v4/users/..",
            "/api/v4/users/%2e%2e/x",
            "/api/v4/channels/./members",
            "/api/v4/users/me//teams?x=1",
        ] {
            let (go, rust) = both(method, target, &[]).await;
            assert_eq!(go.status, 301, "{method} {target}: Go");
            assert_equivalent(&go, &rust, method, target, &[]);
            assert_eq!(rust.served_by.as_deref(), Some("rust"), "{method} {target}");
        }
    }
}
