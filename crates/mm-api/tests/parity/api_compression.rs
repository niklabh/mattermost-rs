//! Cross-server parity for **compression of API answers** — `gzhttp.GzipHandler` around every
//! API handler in `gzip` mode (api4/handlers.go:42), reproduced by `mm_api::go_global_headers`
//! through `mm_api::gzhttp::wrap` (D-208).
//!
//! ```sh
//! scripts/parity.sh --test parity api_compression
//! ```
//!
//! Raw sockets, for the reasons `web_client` gives: reqwest is built here without decompression,
//! and whether an answer has a `Content-Length` or is chunked is part of what is compared. Every
//! header but the per-process ones is compared, and the **decoded** body. A compressed answer's
//! `Content-Length` is not: the two compressors emit different bytes, so the value differs — but
//! each case below is either far below or far above the 2048-byte framing threshold, and its
//! framing is asserted separately.

use super::web_client::{BROWSER_ENCODINGS, Raw, assert_equivalent, raw_request};
use crate::common::{self, GO, RUST, stack_enabled};

async fn both(method: &str, target: &str, headers: &[(&str, &str)]) -> (Raw, Raw) {
    (
        raw_request(GO, method, target, headers).await,
        raw_request(RUST, method, target, headers).await,
    )
}

/// Compare one request on both servers, requiring that Rust answered it itself.
///
/// `X-Version-Id` is dropped from both sides before comparing: `go_global_headers` does not mint
/// it on an API answer (D-207's closing note), which is independent of compression.
async fn assert_served_same(method: &str, target: &str, headers: &[(&str, &str)]) -> (Raw, Raw) {
    let (mut go, mut rust) = both(method, target, headers).await;
    assert_eq!(
        rust.served_by.as_deref(),
        Some("rust"),
        "{method} {target} was forwarded, so the comparison proves nothing"
    );
    go.headers.retain(|(k, _)| k != "x-version-id");
    rust.headers.retain(|(k, _)| k != "x-version-id");
    assert_equivalent(&go, &rust, method, target, headers);
    (go, rust)
}

/// A JSON array's elements, each re-serialised and sorted: the set, whatever the order.
fn as_sorted_list(body: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(body).expect("a JSON body");
    let mut items: Vec<String> = value
        .as_array()
        .expect("a JSON array")
        .iter()
        .map(|v| v.to_string())
        .collect();
    items.sort();
    items
}

/// A handler's JSON, big and small, under every `Accept-Encoding` shape that changes the answer:
/// gzip, the browser list (zstd wins), zstd alone, a refusal, none, and a `HEAD`.
#[tokio::test]
async fn served_json_is_compressed_exactly_when_go_compresses_it() {
    if !stack_enabled() {
        return;
    }
    let http = common::client();
    let token = common::go_minted_token(&http).await;
    let auth = format!("Bearer {token}");

    // `/config/client` is ~5 KB of JSON: compressed, and small enough compressed to get a length.
    let config = "/api/v4/config/client?format=old";
    for ae in ["gzip", BROWSER_ENCODINGS, "zstd", "gzip;q=0", "identity"] {
        let (go, _) = assert_served_same(
            "GET",
            config,
            &[("Authorization", &auth), ("Accept-Encoding", ae)],
        )
        .await;
        let expected = match ae {
            "gzip" => Some("gzip"),
            "zstd" => Some("zstd"),
            a if a == BROWSER_ENCODINGS => Some("zstd"),
            _ => None,
        };
        assert_eq!(go.header("content-encoding"), expected, "{ae}: Go itself");
        assert_eq!(go.header("vary"), Some("Accept-Encoding"), "{ae}");
    }
    assert_served_same("GET", config, &[("Authorization", &auth)]).await;
    // No `HEAD` here: Go's mux has no `HEAD` on an api4 `GET` route and answers 404, where axum
    // answers the `GET` handler's headers — D-1110, not a compression question. That gzhttp never
    // compresses a `HEAD` is pinned by the oracle rows in `mm_api::gzhttp`.

    // Below `MinSize` (1024 bytes): not compressed, `Vary` still there, and a length.
    let (go, rust) = assert_served_same(
        "GET",
        "/api/v4/system/ping",
        &[("Accept-Encoding", BROWSER_ENCODINGS)],
    )
    .await;
    assert!(go.body.len() < 1024, "ping stays below the minimum size");
    assert_eq!(go.header("content-encoding"), None);
    assert!(!rust.chunked);

    // An error answer goes through the same wrapper: the 401 is small, so it is only `Vary`.
    // Its `request_id` is per request, so the bodies are compared without it.
    let target = "/api/v4/users/me";
    let headers = [("Accept-Encoding", "gzip")];
    let (mut go, mut rust) = both("GET", target, &headers).await;
    assert_eq!(rust.served_by.as_deref(), Some("rust"));
    assert_eq!(go.status, 401);
    for raw in [&mut go, &mut rust] {
        raw.headers.retain(|(k, _)| k != "x-version-id");
        let mut error: serde_json::Value = serde_json::from_slice(&raw.body).unwrap();
        error["request_id"] = serde_json::Value::Null;
        raw.body = error.to_string().into_bytes();
    }
    assert_equivalent(&go, &rust, "GET", target, &headers);

    // Far above 2048 compressed bytes: compressed and chunked on both.
    let (go, rust) = assert_served_same(
        "GET",
        "/api/v4/roles",
        &[("Authorization", &auth), ("Accept-Encoding", "gzip")],
    )
    .await;
    assert_eq!(go.header("content-encoding"), Some("gzip"));
    assert!(go.body.len() > 20_000, "roles is large: {}", go.body.len());
    assert_eq!((go.chunked, rust.chunked), (true, true), "framing");
}

/// A file download declares its length and streams; gzhttp decides from the declared length and
/// the type on the first write. A text file over `MinSize` is compressed (its
/// `X-Uncompressed-Content-Length` survives, `Content-Length` and `Accept-Ranges` go), one below
/// it is not, and a range answer (`Content-Range`) never is.
#[tokio::test]
async fn a_file_download_is_compressed_from_its_declared_length() {
    if !stack_enabled() {
        return;
    }
    let http = common::client();
    let token = common::go_minted_token(&http).await;
    let auth = format!("Bearer {token}");
    let channel_id = common::a_channel_the_user_is_in(&http, &token).await;

    let line = b"mattermost compresses a text attachment the client can inflate\n";
    let big: Vec<u8> = line.iter().copied().cycle().take(6000).collect();
    let small: Vec<u8> = line.iter().copied().cycle().take(600).collect();
    let big_id = common::upload_file(
        &http,
        &token,
        &channel_id,
        "gzip-big.txt",
        "text/plain",
        &big,
    )
    .await;
    let small_id = common::upload_file(
        &http,
        &token,
        &channel_id,
        "gzip-small.txt",
        "text/plain",
        &small,
    )
    .await;

    let target = format!("/api/v4/files/{big_id}");
    let (go, rust) = assert_served_same(
        "GET",
        &target,
        &[("Authorization", &auth), ("Accept-Encoding", "gzip")],
    )
    .await;
    assert_eq!(go.header("content-encoding"), Some("gzip"));
    assert_eq!(go.header("accept-ranges"), None);
    assert_eq!(go.header("x-uncompressed-content-length"), Some("6000"));
    assert_eq!(rust.body, big);
    assert!(!rust.chunked, "~100 compressed bytes get a length");

    assert_served_same(
        "GET",
        &target,
        &[
            ("Authorization", &auth),
            ("Accept-Encoding", BROWSER_ENCODINGS),
        ],
    )
    .await;

    let (go, _) = assert_served_same(
        "GET",
        &target,
        &[
            ("Authorization", &auth),
            ("Accept-Encoding", "gzip"),
            ("Range", "bytes=0-99"),
        ],
    )
    .await;
    assert_eq!(go.status, 206);
    assert_eq!(go.header("content-encoding"), None);

    let (go, rust) = assert_served_same(
        "GET",
        &format!("/api/v4/files/{small_id}"),
        &[("Authorization", &auth), ("Accept-Encoding", "gzip")],
    )
    .await;
    assert_eq!(go.header("content-encoding"), None);
    assert_eq!(rust.body, small);
}

/// A forwarded answer is Go's, compressed once: the proxy passes `Accept-Encoding` through and
/// Go's `Content-Encoding` and bytes back, and the wrapper leaves a `go`-marked answer alone.
///
/// `GET /commands` forwards the listing whenever plugin commands could be in it, which on a Go
/// plugin host is always.
#[tokio::test]
async fn a_forwarded_answer_keeps_gos_encoding_and_is_not_compressed_twice() {
    if !stack_enabled() {
        return;
    }
    let http = common::client();
    let token = common::go_minted_token(&http).await;
    let auth = format!("Bearer {token}");
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &token).await;
    let target = format!("/api/v4/commands?team_id={team_id}");

    for ae in [Some("gzip"), Some(BROWSER_ENCODINGS), None] {
        let mut headers = vec![("Authorization", auth.as_str())];
        if let Some(ae) = ae {
            headers.push(("Accept-Encoding", ae));
        }
        let (go, rust) = both("GET", &target, &headers).await;
        assert_eq!(
            rust.served_by.as_deref(),
            Some("go"),
            "{ae:?}: this case is about a forwarded answer"
        );
        assert_eq!(go.status, 200, "{ae:?}");
        assert!(go.body.len() >= 1024, "{ae:?}: large enough to compress");
        assert_eq!(
            rust.header("content-encoding"),
            go.header("content-encoding"),
            "{ae:?}"
        );
        let varies: Vec<_> = rust.headers.iter().filter(|(k, _)| k == "vary").collect();
        assert_eq!(varies.len(), 1, "{ae:?}: one Vary, Go's: {varies:?}");
        // Go lists the commands in map order, so the same set arrives shuffled between calls.
        assert_eq!(
            as_sorted_list(&rust.body),
            as_sorted_list(&go.body),
            "{ae:?}: decoded body"
        );
    }
}
