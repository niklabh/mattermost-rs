//! Cross-server parity for `POST /api/v4/posts` with a **link** in the message: the OpenGraph,
//! image and plain-link previews, the image dimensions, the permalink preview and its
//! `previewed_post` prop, and the `LinkMetadata` row a fetch leaves behind.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_links
//! ```
//!
//! # Two servers of their own, and a website
//!
//! A preview is an outbound fetch through the guard that refuses loopback unless
//! `AllowedUntrustedInternalConnections` names it, and the stack's servers must keep refusing it
//! (`parity::redirect_location` asserts exactly that). So this suite compares
//! `scripts/go-links.sh` — a Go server on Go's port + 50 allowed to reach `127.0.0.1` — with an
//! mm-api given the same setting and the same `SiteURL`, forwarding to that oracle. The website
//! is a mock served from the test process on an ephemeral port, recording every request each
//! server makes of it.
//!
//! Every case posts the **same** URL to both servers, carrying a nonce so neither process's
//! link cache has seen it, and a fixed `create_at` inside one hour, so both servers key the one
//! `LinkMetadata` row the same way. The admin may set `create_at`; that is why the admin posts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, SecondServer, SocketProbe, add_user_to_channel, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    stack_enabled,
};

/// The mm-api compared with the links oracle; see `second_server_ports`.
const LINKS_RUST_PORT: u16 = 8116;

fn go_port() -> u16 {
    GO.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("GO has a port")
}

/// `scripts/go-links.sh port` — Go's port + 50.
fn links_go() -> String {
    format!("http://localhost:{}", go_port() + 50)
}

// ---------------------------------------------------------------------------------------------
// The mock website
// ---------------------------------------------------------------------------------------------

/// One request the website saw: the path and the headers a preview fetch sets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    path: String,
    accept: Vec<String>,
    accept_language: String,
    user_agent: String,
}

#[derive(Default)]
struct Site {
    seen: Vec<Seen>,
    /// `path` → `(status, content type or none, body)`.
    pages: HashMap<String, (u16, Option<String>, Vec<u8>)>,
}

type Shared = Arc<Mutex<Site>>;

async fn serve(
    axum::extract::State(site): axum::extract::State<Shared>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let header = |name: &str| -> Vec<String> {
        request
            .headers()
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect()
    };
    let mut site = site.lock().unwrap();
    site.seen.push(Seen {
        path: path.clone(),
        accept: header("accept"),
        accept_language: header("accept-language").join(","),
        user_agent: header("user-agent").join(","),
    });
    let (status, content_type, body) = site.pages.get(&path).cloned().unwrap_or((
        404,
        Some("text/plain".to_owned()),
        b"no such page".to_vec(),
    ));
    // A 3xx page's body is where it redirects to.
    let location = (300..400)
        .contains(&status)
        .then(|| String::from_utf8_lossy(&body).into_owned());
    let mut response = axum::response::Response::new(axum::body::Body::from(body));
    if let Some(location) = location {
        response.headers_mut().insert(
            axum::http::header::LOCATION,
            axum::http::HeaderValue::from_str(&location).unwrap(),
        );
    }
    *response.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
    // No `Content-Type` at all unless one is given: axum adds none to a raw body.
    if let Some(content_type) = content_type {
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_str(&content_type).unwrap(),
        );
    }
    response
}

struct Website {
    site: Shared,
    base: String,
}

impl Website {
    /// Served from a thread and a runtime of its own: the fixture outlives the test that built
    /// it, and a task spawned on that test's runtime dies with it — every later test then saw
    /// every fetch refused, on both servers alike, and compared two empty previews.
    fn start() -> Website {
        let site: Shared = Arc::default();
        let app = axum::Router::new()
            .fallback(serve)
            .with_state(Arc::clone(&site));
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("the mock website binds");
        listener.set_nonblocking(true).expect("non-blocking");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the website's runtime");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("a tokio listener");
                let _ = axum::serve(listener, app).await;
            });
        });
        Website { site, base }
    }

    fn page(&self, path: &str, status: u16, content_type: Option<&str>, body: &[u8]) {
        self.site.lock().unwrap().pages.insert(
            path.to_owned(),
            (status, content_type.map(str::to_owned), body.to_vec()),
        );
    }

    fn take_seen(&self) -> Vec<Seen> {
        std::mem::take(&mut self.site.lock().unwrap().seen)
    }
}

// ---------------------------------------------------------------------------------------------
// The pair and the fixture
// ---------------------------------------------------------------------------------------------

struct Fixture {
    rust: String,
    _server: SecondServer,
    site: Website,
    team_name: String,
    /// A public channel the admin posts into.
    channel_id: String,
    /// A private channel `reader` is **not** in, holding a post to preview.
    private_id: String,
    reader: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// The website's request log is one list, so the tests of this suite take turns: two cases
/// fetching at once would interleave their requests and neither comparison would hold.
static WEBSITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Panics rather than skips when the oracle is missing, for the reason `common::licensed` gives:
/// a suite whose oracle is absent passes every test while asserting nothing.
async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let go = links_go();
            let up = client
                .get(format!("{go}/api/v4/system/ping"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            assert!(
                up,
                "no links oracle at {go} — start it with `scripts/go-links.sh start`"
            );
            let database_url = std::env::var("DATABASE_URL").unwrap_or_default();
            let server = SecondServer::start(
                LINKS_RUST_PORT,
                &[
                    ("MM_GO_UPSTREAM", go.as_str()),
                    ("MM_SERVICESETTINGS_SITEURL", go.as_str()),
                    (
                        "MM_SERVICESETTINGS_ALLOWEDUNTRUSTEDINTERNALCONNECTIONS",
                        "127.0.0.1",
                    ),
                    ("MM_TEAMSETTINGS_ENABLEOPENSERVER", "true"),
                    ("MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD", "true"),
                    ("MM_SQLSETTINGS_DRIVERNAME", "postgres"),
                    ("MM_SQLSETTINGS_DATASOURCE", database_url.as_str()),
                    ("MM_SERVICESETTINGS_ENABLELOCALMODE", "false"),
                ],
            )
            .await
            .expect("the links mm-api starts — is target/debug/mm-api built?");
            let site = Website::start();
            let team_id = create_team(client, token, "cplk").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cplk", "O").await;
            let private_id = create_channel_typed(client, token, &team_id, "cplkp", "P").await;
            let reader = create_plain_user(client, token, &team_id, "cplk").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            Fixture {
                rust: server.base.clone(),
                _server: server,
                site,
                team_name: "mmrs-parity-cplk".to_owned(),
                channel_id,
                private_id,
                reader,
            }
        })
        .await
}

/// A fresh nonce per call, so neither server's link cache has seen the URL.
fn nonce() -> String {
    format!(
        "{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// A `create_at` both servers accept from the admin, well inside one hour: the start of the
/// current hour plus a minute, so the two posts and the one `LinkMetadata` row share an hour.
fn create_at() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    now - now.rem_euclid(3_600_000) + 60_000
}

async fn create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: serde_json::Value,
) -> (u16, bool, String) {
    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, served, response.text().await.unwrap_or_default())
}

/// The body **text** with the post's id and clocks blanked. Text, not a parsed value, because an
/// OpenGraph embed's key order is Go's struct order, which a `serde_json::Value` would sort away.
fn normalised_text(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
    let mut text = body.to_owned();
    if let Some(id) = value["id"].as_str() {
        text = text.replacen(&format!("\"id\":\"{id}\""), "\"id\":\"\"", 1);
    }
    for key in ["create_at", "update_at"] {
        if let Some(n) = value[key].as_i64() {
            text = text.replacen(&format!("\"{key}\":{n}"), &format!("\"{key}\":0"), 1);
        }
    }
    text
}

type LinkRow = (String, i64, String, Option<serde_json::Value>);

/// The `LinkMetadata` row for `url` at `hour`, if any.
async fn link_row(url: &str, hour: i64) -> Option<LinkRow> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        r#"SELECT url, "timestamp", type, data FROM linkmetadata WHERE url = $1 AND "timestamp" = $2"#,
    )
    .bind(url)
    .bind(hour)
    .fetch_optional(&pool)
    .await
    .expect("the link metadata query")
}

async fn delete_link_rows(urls: &[String]) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query("DELETE FROM linkmetadata WHERE url = ANY($1)")
        .bind(urls)
        .execute(&pool)
        .await
        .expect("the link metadata purge");
}

/// The `Props` column of a saved post.
async fn saved_props(post_id: &str) -> serde_json::Value {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let props: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT props FROM posts WHERE id = $1")
            .bind(post_id)
            .fetch_one(&pool)
            .await
            .expect("the post row");
    props.unwrap_or(serde_json::Value::Null)
}

/// Post `message` to both servers as the admin with a fixed `create_at`, and assert the two
/// answers, the requests each made of the website, and the `LinkMetadata` rows they left for
/// `fetched` agree. Returns Go's body.
async fn compare(
    client: &reqwest::Client,
    token: &str,
    f: &Fixture,
    message: &str,
    fetched: &[String],
) -> (serde_json::Value, Vec<Option<LinkRow>>) {
    let at = create_at();
    let hour = at - at.rem_euclid(3_600_000);
    delete_link_rows(fetched).await;
    f.site.take_seen();

    let mut texts = Vec::new();
    let mut requests = Vec::new();
    let mut rows = Vec::new();
    let mut go_body = serde_json::Value::Null;
    for base in [links_go(), f.rust.clone()] {
        let (status, served, body) = create(
            client,
            &base,
            token,
            serde_json::json!({
                "channel_id": f.channel_id,
                "message": message,
                "create_at": at,
            }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == f.rust, "{base}: served by");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["create_at"], at, "{base}: the admin's create_at");
        if go_body.is_null() {
            go_body = value;
        }
        texts.push(normalised_text(&body));
        requests.push(f.site.take_seen());
        let mut these = Vec::new();
        for url in fetched {
            these.push(link_row(url, hour).await);
        }
        rows.push(these);
        // Each server writes the row afresh; the second must not merely find the first's.
        delete_link_rows(fetched).await;
    }
    assert_eq!(
        texts[0], texts[1],
        "the create bodies differ for {message:?}"
    );
    assert_eq!(
        requests[0], requests[1],
        "the two servers fetched differently for {message:?}"
    );
    assert_eq!(
        rows[0], rows[1],
        "the LinkMetadata rows differ for {message:?}"
    );
    (go_body, rows.swap_remove(0))
}

// ---------------------------------------------------------------------------------------------
// The website's content
// ---------------------------------------------------------------------------------------------

/// A 3×2 RGB PNG.
fn png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc_input);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&3u32.to_be_bytes());
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    // A stored (uncompressed) zlib stream of two scanlines, filter byte 0 and nine zero bytes.
    let raw = [0u8; 2 * (1 + 9)];
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    zlib.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A 5×4 GIF of two frames, each an empty 1×1 image.
fn animated_gif() -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&5u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    // Global colour table of two entries.
    out.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
    for _ in 0..2 {
        // Graphic control extension, then a 1×1 image with LZW minimum code size 2.
        out.extend_from_slice(&[0x21, 0xF9, 4, 0, 10, 0, 0, 0]);
        out.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]);
        out.extend_from_slice(&[2, 2, 0x44, 0x01, 0]);
    }
    out.push(0x3B);
    out
}

/// A JPEG from `fixtures/behaviour_link_image.json` whose EXIF orientation swaps the sides.
fn rotated_jpeg() -> (Vec<u8>, i64, i64) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/behaviour_link_image.json"
    ))
    .expect("generated by reference/dump");
    let case = fixture["images"]
        .as_array()
        .expect("the corpus")
        .iter()
        .find(|c| c["name"] == "jpeg exif le orientation 6")
        .expect("the orientation-6 JPEG");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(case["input"].as_str().expect("base64"))
        .expect("base64");
    (
        bytes,
        case["width"].as_i64().expect("a width"),
        case["height"].as_i64().expect("a height"),
    )
}

fn og_page(n: &str) -> String {
    let long = "é".repeat(310);
    format!(
        r#"<!DOCTYPE html><html><head>
<title>ignored</title>
<meta property="og:title" content="Links &amp;amp; things">
<meta property="og:type" content="article">
<meta property="og:url" content="/canonical/{n}">
<meta property="og:site_name" content="Mock">
<meta property="og:description" content="{long}">
<meta property="og:locale" content="en_GB">
<meta property="og:locale:alternate" content="fr_FR">
<meta property="og:image" content="/img/{n}/z.png">
<meta property="og:image:width" content="300">
<meta property="og:image:height" content="200">
<meta property="og:image" content="https://cdn.invalid/b.svg">
<meta property="og:image" content="/img/{n}/plain.png">
<meta property="og:image:secure_url" content="/img/{n}/anim.gif">
<meta property="og:article:author" content="someone">
<meta property="og:audio" content="/a.mp3">
<script><meta property="og:title" content="not this"></script>
</head><body>hi</body></html>"#
    )
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// An OpenGraph page: the embed carries the truncated document in Go's field order — the title
/// unescaped twice, the description cut at 300 runes, `article`, `audios` and the locales
/// blanked, the SVG image dropped, relative URLs made absolute and `og:url` replaced by the
/// link — and `images` measures the page's images through the secure URL where there is one,
/// fetched in **sorted** order (`RemoveDuplicateStrings`): `anim.gif` before `z.png`, the reverse
/// of the page's.
#[tokio::test]
async fn an_opengraph_page_is_previewed_and_its_images_measured() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    f.site.page(
        &format!("/og/{n}"),
        200,
        Some("text/html; charset=utf-8"),
        og_page(&n).as_bytes(),
    );
    f.site
        .page(&format!("/img/{n}/z.png"), 200, Some("image/png"), &png());
    f.site.page(
        &format!("/img/{n}/anim.gif"),
        200,
        Some("image/gif"),
        &animated_gif(),
    );
    let link = format!("{}/og/{n}", f.site.base);
    let a = format!("{}/img/{n}/z.png", f.site.base);
    let gif = format!("{}/img/{n}/anim.gif", f.site.base);
    let (body, _) = compare(
        &client,
        &token,
        f,
        &format!("cplk og {link}"),
        &[link.clone(), a.clone(), gif.clone()],
    )
    .await;

    let embed = &body["metadata"]["embeds"][0];
    assert_eq!(embed["type"], "opengraph", "{body}");
    assert_eq!(embed["url"], link.as_str());
    assert_eq!(embed["data"]["title"], "Links & things", "unescaped twice");
    assert_eq!(embed["data"]["url"], link.as_str(), "og:url is the link");
    assert!(
        embed["data"].get("article").is_none(),
        "blanked by truncation"
    );
    assert_eq!(body["metadata"]["images"][&a]["width"], 3);
    assert_eq!(body["metadata"]["images"][&gif]["frame_count"], 2);
}

/// An image link is an `image` embed with its dimensions under `images`; a JPEG's EXIF rotation
/// swaps them; a markdown image is measured without an embed.
#[tokio::test]
async fn an_image_link_and_a_markdown_image_are_measured() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    let (jpeg, width, height) = rotated_jpeg();
    f.site
        .page(&format!("/img/{n}/p.png"), 200, Some("image/png"), &png());
    f.site
        .page(&format!("/img/{n}/r.jpg"), 200, Some("image/jpeg"), &jpeg);
    let png_url = format!("{}/img/{n}/p.png", f.site.base);
    let jpg_url = format!("{}/img/{n}/r.jpg", f.site.base);

    let (body, _) = compare(
        &client,
        &token,
        f,
        &format!("cplk image {png_url}"),
        std::slice::from_ref(&png_url),
    )
    .await;
    assert_eq!(body["metadata"]["embeds"][0]["type"], "image", "{body}");
    assert_eq!(body["metadata"]["images"][&png_url]["height"], 2);

    let (body, _) = compare(
        &client,
        &token,
        f,
        &format!("cplk markdown ![rotated]({jpg_url})"),
        std::slice::from_ref(&jpg_url),
    )
    .await;
    assert!(body["metadata"].get("embeds").is_none(), "{body}");
    assert_eq!(
        (
            body["metadata"]["images"][&jpg_url]["width"].as_i64(),
            body["metadata"]["images"][&jpg_url]["height"].as_i64()
        ),
        (Some(width), Some(height)),
        "the oracle's own swapped dimensions"
    );
}

/// The shapes with nothing to preview: a page with no OpenGraph, a type that is neither image
/// nor HTML, an `image/svg+xml` answer, a PNG behind an `.svg` URL (a `link` embed, yet still
/// measured — the fresh path does not filter SVG URLs out of `images`), a body with no
/// `Content-Type` sniffed as an image, a 404 page with tags (previewed: the status is never
/// read), and a redirect followed to the page — whose embed keeps the link, not the target.
#[tokio::test]
async fn links_with_little_or_odd_metadata_agree() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    let base = f.site.base.clone();
    f.site.page(
        &format!("/bare/{n}"),
        200,
        Some("text/html"),
        b"<html><head><title>no og</title></head></html>",
    );
    f.site
        .page(&format!("/text/{n}"), 200, Some("text/plain"), b"just text");
    f.site.page(
        &format!("/svgtype/{n}"),
        200,
        Some("image/svg+xml"),
        b"<svg xmlns='http://www.w3.org/2000/svg'/>",
    );
    f.site
        .page(&format!("/img/{n}/x.svg"), 200, Some("image/png"), &png());
    f.site.page(&format!("/sniff/{n}"), 200, None, &png());
    f.site.page(
        &format!("/gone/{n}"),
        404,
        Some("text/html"),
        br#"<meta property="og:title" content="Missing">"#,
    );
    f.site.page(
        &format!("/moved/{n}"),
        200,
        Some("text/html"),
        br#"<meta property="og:type" content="website">"#,
    );

    f.site.page(
        &format!("/redirect/{n}"),
        302,
        Some("text/plain"),
        format!("/moved/{n}").as_bytes(),
    );

    for (path, expected) in [
        (format!("/bare/{n}"), "link"),
        (format!("/text/{n}"), "link"),
        (format!("/svgtype/{n}"), "link"),
        (format!("/img/{n}/x.svg"), "link"),
        (format!("/sniff/{n}"), "image"),
        (format!("/gone/{n}"), "opengraph"),
        (format!("/redirect/{n}"), "opengraph"),
    ] {
        let url = format!("{base}{path}");
        let (body, _) = compare(
            &client,
            &token,
            f,
            &format!("cplk odd {url}"),
            std::slice::from_ref(&url),
        )
        .await;
        assert_eq!(
            body["metadata"]["embeds"][0]["type"], expected,
            "{path}: {body}"
        );
    }
}

/// The cached path filters what the fresh one does not: a PNG behind an `.svg` URL is measured
/// the first time (`getLinkMetadataForURL` never looks at the URL) and dropped the second, when
/// the answer comes from the process's link cache through `filterSVGImage`. Each server has its
/// own cache and both take the same two paths.
#[tokio::test]
async fn an_svg_url_is_measured_fresh_and_filtered_from_the_cache() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    f.site
        .page(&format!("/img/{n}/m.svg"), 200, Some("image/png"), &png());
    let url = format!("{}/img/{n}/m.svg", f.site.base);
    let message = format!("cplk svg ![svg]({url})");
    let (fresh, _) = compare(&client, &token, f, &message, std::slice::from_ref(&url)).await;
    assert_eq!(
        fresh["metadata"]["images"][&url]["format"], "png",
        "{fresh}"
    );
    let (cached, _) = compare(&client, &token, f, &message, std::slice::from_ref(&url)).await;
    assert!(cached["metadata"].get("images").is_none(), "{cached}");
}

/// A link nothing answers: no embed at all — `getEmbedForPost` returns the fetch's error — and
/// still a `none` row, "because we want to save that there is no metadata for this link".
#[tokio::test]
async fn an_unreachable_link_leaves_no_embed_and_a_none_row() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    // Port 9 (discard) on loopback: refused at once on every machine this runs on.
    let url = format!("http://127.0.0.1:9/cplk/{}", nonce());
    let at = create_at();
    let hour = at - at.rem_euclid(3_600_000);
    let (body, rows) = compare(
        &client,
        &token,
        f,
        &format!("cplk refused {url}"),
        std::slice::from_ref(&url),
    )
    .await;
    assert!(body["metadata"].get("embeds").is_none(), "{body}");
    let row = rows[0].clone().expect("a none row");
    assert_eq!(
        (row.0.as_str(), row.1, row.2.as_str(), row.3),
        (url.as_str(), hour, "none", Some(serde_json::Value::Null))
    );
}

/// A permalink is previewed from the database: the embed carries the referenced post, its
/// channel and team; the saved row carries `previewed_post`; a reader of the channel gets the
/// preview in the `posted` frame, and a reader of a post in a channel they cannot read gets
/// neither the embed nor the prop — in the response when they are the author, in the frame
/// when they are a recipient.
#[tokio::test]
async fn a_permalink_is_previewed_for_those_who_may_read_it() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();

    // The referenced posts, written through the main Go server.
    let mut referenced = Vec::new();
    for channel in [&f.channel_id, &f.private_id] {
        let (status, _, body) = create(
            &client,
            GO,
            &token,
            serde_json::json!({ "channel_id": channel, "message": format!("cplk referenced {n}") }),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        referenced.push(value["id"].as_str().unwrap().to_owned());
    }
    let permalink = |id: &str| format!("{}/{}/pl/{id}", links_go(), f.team_name);

    // The admin previews the public post; the reader receives it.
    let message = format!("cplk permalink {}", permalink(&referenced[0]));
    let mut bodies = Vec::new();
    let mut frames = Vec::new();
    for base in [links_go(), f.rust.clone()] {
        let mut reader_socket = SocketProbe::connect(&base, &f.reader.token).await;
        let (status, served, body) = create(
            &client,
            &base,
            &token,
            serde_json::json!({ "channel_id": f.channel_id, "message": message }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == f.rust, "{base}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let post_id = value["id"].as_str().unwrap().to_owned();
        assert_eq!(
            saved_props(&post_id).await["previewed_post"],
            referenced[0].as_str(),
            "{base}: the row carries the prop"
        );
        let wanted = post_id.clone();
        assert!(
            reader_socket
                .collect_until(Duration::from_millis(2500), move |frames| {
                    frames.iter().any(|f| {
                        f["event"] == "posted"
                            && f["data"]["post"]
                                .as_str()
                                .is_some_and(|p| p.contains(&wanted))
                    })
                })
                .await,
            "{base}: no posted frame"
        );
        let frame = reader_socket
            .events_named("posted")
            .into_iter()
            .find(|f| {
                f["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains(&post_id))
            })
            .unwrap();
        let mut frame_post: serde_json::Value =
            serde_json::from_str(frame["data"]["post"].as_str().unwrap()).unwrap();
        frame_post["id"] = serde_json::json!("");
        frame_post["create_at"] = serde_json::json!(0);
        frame_post["update_at"] = serde_json::json!(0);
        frames.push(frame_post);
        bodies.push(normalised_text(&body));
    }
    assert_eq!(bodies[0], bodies[1], "the permalink create bodies differ");
    assert_eq!(frames[0], frames[1], "the reader's posted frames differ");
    let go: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(go["metadata"]["embeds"][0]["type"], "permalink", "{go}");
    assert_eq!(
        frames[0]["metadata"]["embeds"][0]["type"], "permalink",
        "the reader may read the channel: {}",
        frames[0]
    );

    // The reader previews the private post: written with the prop, answered without either.
    let message = format!("cplk hidden {}", permalink(&referenced[1]));
    let mut bodies = Vec::new();
    for base in [links_go(), f.rust.clone()] {
        let (status, served, body) = create(
            &client,
            &base,
            &f.reader.token,
            serde_json::json!({ "channel_id": f.channel_id, "message": message }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == f.rust, "{base}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            saved_props(value["id"].as_str().unwrap()).await["previewed_post"],
            referenced[1].as_str(),
            "{base}: the row still carries the prop"
        );
        assert!(
            value["props"].get("previewed_post").is_none(),
            "{base}: {value}"
        );
        bodies.push(normalised_text(&body));
    }
    assert_eq!(bodies[0], bodies[1], "the hidden-permalink bodies differ");

    // A permalink to nothing: no embed, no prop, and nothing fetched.
    let message = format!("cplk nowhere {}", permalink("zzzzzzzzzzzzzzzzzzzzzzzzzz"));
    let mut bodies = Vec::new();
    for base in [links_go(), f.rust.clone()] {
        let (status, _, body) = create(
            &client,
            &base,
            &token,
            serde_json::json!({ "channel_id": f.channel_id, "message": message }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        bodies.push(normalised_text(&body));
    }
    assert_eq!(bodies[0], bodies[1], "the dangling-permalink bodies differ");
}
