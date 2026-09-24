//! Cross-server parity for the **web client**: `/static/*`, the SPA page for every other path,
//! `/robots.txt` and `/unsupported_browser.js` — `mm_api::web_static`.
//!
//! ```sh
//! scripts/parity.sh --test parity web_client
//! ```
//!
//! # Raw sockets, not reqwest
//!
//! Three things reqwest would hide are the point here. It resolves `..` segments before sending
//! (so gorilla's clean-path redirect is never exercised), it decides framing for us (and whether
//! a response had a `Content-Length` or was chunked is `net/http`'s 2048-byte rule, which the port
//! reproduces), and it is built without decompression here, so a compressed body would have to be
//! decoded by hand anyway. So each request is written by hand on a fresh connection with
//! `Connection: close`, and the response is parsed from the bytes.
//!
//! # What is compared
//!
//! Status, every header but the per-process ones (`Date`, `X-Request-Id`, the port's own marker,
//! the connection headers), and the **decoded** body. `X-Version-Id` *is* compared: it carries the
//! client-config hash, so it agrees only if the port's client config is Go's to the byte.
//!
//! One header is not compared: a compressed response's `Content-Length`. Go's compressor and
//! ours emit different bytes for the same input, so a body small enough to get a length (at most
//! 2048 compressed bytes) has a different one — and one near that boundary may get a length on
//! one side and be chunked on the other, so neither the value nor the framing is asserted there.
//!
//! # Which assets
//!
//! Whatever the build has. The paths come from Go's own `root.html` (every `/static/` reference in
//! it), so the suite follows the webapp through rebuilds; a stack without a built webapp has no
//! `root.html`, and then the page itself is the parity case — both servers' 500 naming the missing
//! file (D-782's off-branch).

use crate::common::{self, GO, RUST, SecondServer, stack_enabled};

/// Per-process headers, never comparable.
const VOLATILE: [&str; 6] = [
    "date",
    "x-request-id",
    "x-mmrs-served-by",
    "connection",
    "keep-alive",
    "transfer-encoding",
];

/// What every current Firefox and Chrome send — and so what selects zstd.
pub(crate) const BROWSER_ENCODINGS: &str = "gzip, deflate, br, zstd";

#[derive(Debug)]
pub(crate) struct Raw {
    pub(crate) status: u16,
    /// Lower-cased names, sorted, volatile ones removed.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) served_by: Option<String>,
    pub(crate) chunked: bool,
    /// Decoded according to `Content-Encoding`.
    pub(crate) body: Vec<u8>,
}

impl Raw {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn host_port(base: &str) -> String {
    base.trim_start_matches("http://").to_owned()
}

pub(crate) async fn raw_request(
    base: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> Raw {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = host_port(base);
    let mut stream = tokio::net::TcpStream::connect(&addr)
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stream.read_to_end(&mut bytes),
    )
    .await
    .expect("the response finished within 30s")
    .unwrap();
    parse_response(&bytes, method == "HEAD")
}

fn parse_response(bytes: &[u8], head: bool) -> Raw {
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header block");
    let head_text = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let mut rest = &bytes[split + 4..];
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut all: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    all.sort();
    let chunked = all
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let served_by = all
        .iter()
        .find(|(k, _)| k == "x-mmrs-served-by")
        .map(|(_, v)| v.clone());

    let mut body = Vec::new();
    if !head {
        if chunked {
            loop {
                let line_end = rest.windows(2).position(|w| w == b"\r\n").unwrap();
                let size_text = String::from_utf8_lossy(&rest[..line_end]);
                let size =
                    usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16).unwrap();
                rest = &rest[line_end + 2..];
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&rest[..size]);
                rest = &rest[size + 2..];
            }
        } else {
            body = rest.to_vec();
        }
    }
    let encoding = all
        .iter()
        .find(|(k, _)| k == "content-encoding")
        .map(|(_, v)| v.clone());
    let body = match encoding.as_deref() {
        Some("gzip") => {
            use std::io::Read as _;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(body.as_slice())
                .read_to_end(&mut out)
                .unwrap();
            out
        }
        Some("zstd") => zstd::stream::decode_all(body.as_slice()).unwrap(),
        _ => body,
    };
    let headers = all
        .into_iter()
        .filter(|(k, _)| !VOLATILE.contains(&k.as_str()))
        .collect();
    Raw {
        status,
        headers,
        served_by,
        chunked,
        body,
    }
}

/// Compare one request on both servers and require that Rust answered it itself.
async fn assert_same(method: &str, target: &str, headers: &[(&str, &str)]) -> (Raw, Raw) {
    let (go, rust) = both(method, target, headers).await;
    assert_equivalent(&go, &rust, method, target, headers);
    assert_eq!(
        rust.served_by.as_deref(),
        Some("rust"),
        "{method} {target} {headers:?} was forwarded, so the comparison proves nothing"
    );
    (go, rust)
}

async fn both(method: &str, target: &str, headers: &[(&str, &str)]) -> (Raw, Raw) {
    (
        raw_request(GO, method, target, headers).await,
        raw_request(RUST, method, target, headers).await,
    )
}

pub(crate) fn assert_equivalent(
    go: &Raw,
    rust: &Raw,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) {
    let context = format!("{method} {target} {headers:?}");
    assert_eq!(go.status, rust.status, "{context}: status");
    let compressed = go.header("content-encoding").is_some();
    let strip_length = |h: &[(String, String)]| -> Vec<(String, String)> {
        h.iter()
            .filter(|(k, _)| !(compressed && k == "content-length"))
            .cloned()
            .collect()
    };
    assert_eq!(
        strip_length(&go.headers),
        strip_length(&rust.headers),
        "{context}: headers"
    );
    assert_eq!(go.body, rust.body, "{context}: decoded body");
    if !compressed {
        assert_eq!(go.chunked, rust.chunked, "{context}: framing");
    }
}

/// Every `/static/…` path Go's `root.html` names, deduplicated, in order of appearance.
fn static_references(root_html: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(root_html);
    let mut out: Vec<String> = Vec::new();
    for (at, _) in text.match_indices("/static/") {
        let tail = &text[at..];
        let end = tail
            .find(['"', '\'', ' ', ')', '>', '<', '?', '#'])
            .unwrap_or(tail.len());
        let path = &tail[..end];
        if path.len() > "/static/".len() && !out.iter().any(|p| p == path) {
            out.push(path.to_owned());
        }
    }
    out
}

/// One asset per extension, so the sample covers every content type the build ships without
/// fetching all of them.
fn one_per_extension(paths: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for path in paths {
        let ext = path.rsplit_once('.').map_or("", |(_, e)| e).to_owned();
        if !seen.contains(&ext) {
            seen.push(ext);
            out.push(path.clone());
        }
    }
    out
}

#[tokio::test]
async fn the_spa_page_is_gos_for_every_page_path() {
    if !stack_enabled() {
        return;
    }
    for target in [
        "/",
        "/login",
        "/team/channels/town-square",
        "/team/channels/town-square?view=info&x=%41",
        "/robots.txt/",
        "/static",
        "/hooks/abc/",
        "/hooks/abc",
        "/signup_user_complete",
        "/login/desktop",
    ] {
        for method in ["GET", "HEAD"] {
            assert_same(method, target, &[]).await;
            assert_same(method, target, &[("Accept-Encoding", BROWSER_ENCODINGS)]).await;
        }
    }
    // A modern Safari and a current Edge are served; the floor is 12 for both families.
    for ua in [
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
        "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    ] {
        assert_same("GET", "/login", &[("User-Agent", ua)]).await;
    }
}

/// A browser session carries its cookie on the page request. `GetSession` runs (and so do its
/// side effects) but a session is not required, so a bad cookie is the same page.
#[tokio::test]
async fn a_cookie_good_or_bad_does_not_change_the_page() {
    if !stack_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let good = format!("MMAUTHTOKEN={token}");
    assert_same("GET", "/team/channels/town-square", &[("Cookie", &good)]).await;
    assert_same(
        "GET",
        "/team/channels/town-square",
        &[("Cookie", "MMAUTHTOKEN=abcdefghijklmnopqrstuvwxyz")],
    )
    .await;
}

#[tokio::test]
async fn built_assets_are_served_byte_for_byte_with_gos_caching_headers() {
    if !stack_enabled() {
        return;
    }
    let page = raw_request(GO, "GET", "/", &[]).await;
    if page.status != 200 {
        // No webapp build on this stack: the page itself is the case, and it is covered above.
        return;
    }
    let references = static_references(&page.body);
    assert!(
        references.len() > 3,
        "root.html names its bundles: {references:?}"
    );
    for path in one_per_extension(&references) {
        for method in ["GET", "HEAD"] {
            assert_same(method, &path, &[]).await;
            assert_same(method, &path, &[("Accept-Encoding", BROWSER_ENCODINGS)]).await;
            assert_same(method, &path, &[("Accept-Encoding", "gzip")]).await;
        }
        assert_same("GET", &path, &[("Accept-Encoding", "gzip;q=0, zstd;q=0")]).await;
        assert_same("GET", &format!("{path}?cache-buster=1"), &[]).await;

        // The browser's revalidation — every `/static/` request of a warm session is this.
        let (go, _) = assert_same("GET", &path, &[]).await;
        let last_modified = go
            .header("last-modified")
            .expect("a file has a modification time")
            .to_owned();
        let (not_modified, _) = assert_same(
            "GET",
            &path,
            &[
                ("If-Modified-Since", &last_modified),
                ("Accept-Encoding", BROWSER_ENCODINGS),
            ],
        )
        .await;
        assert_eq!(not_modified.status, 304, "{path} revalidates");
        assert_same("HEAD", &path, &[("If-Modified-Since", &last_modified)]).await;
        assert_same(
            "GET",
            &path,
            &[("If-Modified-Since", "Mon, 01 Jan 2001 00:00:00 GMT")],
        )
        .await;
        assert_same("GET", &path, &[("If-None-Match", "*")]).await;
        assert_same("GET", &path, &[("If-Match", "\"x\"")]).await;
        assert_same(
            "GET",
            &path,
            &[("If-Unmodified-Since", "Mon, 01 Jan 2001 00:00:00 GMT")],
        )
        .await;
        assert_same(
            "GET",
            &path,
            &[
                ("Range", "bytes=0-9"),
                ("Accept-Encoding", BROWSER_ENCODINGS),
            ],
        )
        .await;
        assert_same("GET", &path, &[("Range", "bytes=99999999-")]).await;
        assert_same(
            "GET",
            &path,
            &[("Range", "bytes=0-9"), ("If-Range", &last_modified)],
        )
        .await;
    }
}

#[tokio::test]
async fn misses_directories_and_traversal_get_gos_answers() {
    if !stack_enabled() {
        return;
    }
    for target in [
        // A plugin's module-federation entry point: revalidated, not cached for a year.
        "/static/remote_entry.js",
        "/static/no-such-bundle.js",
        "/static/no-such-dir/x.js",
        "/static/images",
        "/static/images?x=1",
        "/static/images/",
        "/static/",
        "/static/index.html",
        "/static/images/index.html?q=1",
        "/static/root.html/x",
        "/static/plugins/no-such-plugin/x.js",
        "/static/plugins/",
        "/static/%2e%2e/%2e%2e/etc/passwd",
        "/static/images/../root.html",
        "/static/../../etc/passwd",
        "/static//images",
        "/%73tatic/root.html",
        "/static/%00.js",
        "/a/./b/../c",
    ] {
        for method in ["GET", "HEAD"] {
            assert_same(method, target, &[]).await;
            assert_same(method, target, &[("Accept-Encoding", BROWSER_ENCODINGS)]).await;
        }
    }
}

#[tokio::test]
async fn robots_and_the_unsupported_browser_script_are_bare_handlers() {
    if !stack_enabled() {
        return;
    }
    for target in ["/robots.txt", "/unsupported_browser.js"] {
        for method in ["GET", "HEAD"] {
            assert_same(method, target, &[]).await;
            assert_same(method, target, &[("Accept-Encoding", BROWSER_ENCODINGS)]).await;
        }
    }
    let (go, _) = assert_same("GET", "/unsupported_browser.js", &[]).await;
    let last_modified = go.header("last-modified").unwrap().to_owned();
    assert_same(
        "GET",
        "/unsupported_browser.js",
        &[("If-Modified-Since", &last_modified)],
    )
    .await;
}

/// `root`'s `IsAPICall` branch: an `/api/` path no api4 route owns is the JSON 404, with the
/// static handler's headers on it. The message is Go's translation and ours the id ([D-092]),
/// which also moves `Content-Length`.
#[tokio::test]
async fn an_unknown_api_path_outside_api4_is_the_json_404() {
    if !stack_enabled() {
        return;
    }
    for target in ["/api/v3/x", "/api/nope/", "/api/"] {
        let (go, rust) = both("GET", target, &[]).await;
        assert_eq!(go.status, 404, "{target}");
        assert_eq!(rust.status, 404, "{target}");
        assert_eq!(rust.served_by.as_deref(), Some("rust"), "{target}");
        let without_length = |h: &[(String, String)]| -> Vec<(String, String)> {
            h.iter()
                .filter(|(k, _)| k != "content-length")
                .cloned()
                .collect()
        };
        assert_eq!(
            without_length(&go.headers),
            without_length(&rust.headers),
            "{target}"
        );
        common::assert_error_bodies_match_except_known_gaps(&go.body, &rust.body, target);
    }
}

/// What this port leaves to Go: other handlers' paths, other methods, the template pages.
#[tokio::test]
async fn what_is_not_the_web_clients_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let old_safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_13_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/11.1.2 Safari/605.1.15";
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
    let cases: [Case; 6] = [
        ("GET", "/plugins/com.example.none/x", &[]),
        ("GET", "/api/v5/anything", &[]),
        ("GET", "/login/sso/saml", &[]),
        ("POST", "/login", &[]),
        ("GET", "/login", &[("User-Agent", old_safari)]),
        ("GET", "/?access_token=abcdefghijklmnopqrstuvwxyz", &[]),
    ];
    for (method, target, headers) in cases {
        let (go, rust) = both(method, target, headers).await;
        assert_eq!(
            rust.served_by.as_deref(),
            Some("go"),
            "{method} {target} must be forwarded"
        );
        // Including a redirect, which the forward leg used to follow (`AppState::forward_http`).
        assert_eq!(go.status, rust.status, "{method} {target}");
        // `RenderWebError`'s `&s=` is an ECDSA signature with a random nonce: it differs between
        // two calls to Go itself.
        let unsigned = |raw: &Raw| {
            raw.header("location")
                .map(|l| l.split("&s=").next().unwrap_or_default().to_owned())
        };
        assert_eq!(unsigned(&go), unsigned(&rust), "{method} {target}");
    }
}

/// D-903: a forwarded `HEAD` is framed as Go framed it. The proxy rebuilds every body, and a
/// `HEAD` has none, so Go's `Content-Length` — the entity's length — must be carried over rather
/// than recomputed as `0`; and where Go sent none (its handler wrote nothing), none is invented.
#[tokio::test]
async fn a_forwarded_head_keeps_gos_content_length() {
    if !stack_enabled() {
        return;
    }
    let mut sized = 0;
    for target in [
        "/api/v5/x",
        "/api/v4/no-such-route",
        "/plugins/com.example.none/x",
        "/login/sso/saml",
        "/oauth/authorize",
        "/?access_token=abcdefghijklmnopqrstuvwxyz",
    ] {
        let (go, rust) = both("HEAD", target, &[]).await;
        assert_eq!(
            rust.served_by.as_deref(),
            Some("go"),
            "HEAD {target} must be forwarded, or this compares nothing"
        );
        assert_eq!(go.status, rust.status, "HEAD {target}");
        assert_eq!(
            go.header("content-length"),
            rust.header("content-length"),
            "HEAD {target}: Content-Length"
        );
        assert_eq!(go.chunked, rust.chunked, "HEAD {target}: framing");
        sized += usize::from(go.header("content-length").is_some_and(|l| l != "0"));
    }
    // Both branches must be exercised: a length Go stated, and none at all.
    assert!(sized >= 2, "at least two answers carry a non-zero length");
    assert!(sized < 6, "at least one answer carries no length");
}

/// D-782's off-branch with no client directory at all: a server whose working directory has no
/// `client/` anywhere above it reads `root.html` from `./` and answers Go's 500 with the
/// `*PathError` text. The Go side is not comparable — the stack's Go has a client directory — so
/// this pins the Rust answer against the text `os.ReadFile` produces.
#[tokio::test]
async fn with_no_client_directory_the_page_is_gos_500() {
    if !stack_enabled() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mmrs-web-noclient-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No `client/` is the subject; no `i18n/` would merely stop the server booting, and
    // `start_in` would then answer `None` and this test would pass having asserted nothing.
    let _ = std::os::unix::fs::symlink(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n"),
        dir.join("i18n"),
    );
    let Some(server) = SecondServer::start_in(8117, &dir, &[]).await else {
        return;
    };
    let answer = raw_request(&server.base, "GET", "/login", &[]).await;
    assert_eq!(answer.status, 500);
    assert_eq!(
        String::from_utf8_lossy(&answer.body),
        "open root.html: no such file or directory\n"
    );
    assert_eq!(
        answer.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        answer.header("cache-control"),
        Some("no-cache, max-age=31556926, public")
    );
    assert_eq!(answer.header("x-frame-options"), Some("SAMEORIGIN"));
    assert_eq!(answer.header("content-length"), Some("42"));
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
