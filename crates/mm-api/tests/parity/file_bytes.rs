//! Cross-server parity for the routes that return **file bytes**, and for the four that return a
//! stored image.
//!
//! ```sh
//! docker compose up -d && scripts/go-server.sh start
//! scripts/parity.sh -p mm-api --test parity file_bytes
//! ```
//!
//! # Both servers must be looking at the same directory
//!
//! The file backend is a path on this host, and the Go server is started by `scripts/go-server.sh`
//! with `MM_FILESETTINGS_DIRECTORY` pointing inside `reference/.build/mmroot`. That is an
//! *environment* override, so it never reaches the configuration document mm-api reads —
//! `scripts/parity.sh` exports the same variable for mm-api for exactly that reason. Without it
//! every assertion here fails with a 404 from Rust and a 200 from Go, and the cause is not in
//! either handler.
//!
//! # What is compared
//!
//! Status, **every response header** and the body — not just the body. Most of what
//! `web.WriteFileResponse` does is headers, and three of its answers (304, 412, 416) are defined
//! by which headers they *delete*. A body-only comparison would pass on all of them.

use crate::common;

use common::{
    GO, RUST, TINY_PNG, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_custom_emoji, create_team, fetch_both_raw, go_minted_token, logged_in_user_id,
    post_message_with_files, purge_api_fixtures, stack_enabled, unique_emoji_name, upload_file,
};

/// Headers that cannot agree between two processes and are excluded from every comparison.
///
/// `date` moves, `x-mmrs-served-by` is this port's own marker with no Go counterpart, and
/// `x-request-id`/`x-version-id` are per-server identity. Everything else is compared, including
/// the ones a reader would not think to check.
const VOLATILE_HEADERS: [&str; 6] = [
    "date",
    "x-mmrs-served-by",
    "x-request-id",
    "x-version-id",
    "connection",
    "transfer-encoding",
];

/// One request to both servers, returning `(status, sorted headers, body)` for each.
async fn fetch_both_full(
    client: &reqwest::Client,
    token: Option<&str>,
    method: reqwest::Method,
    path: &str,
    headers: &[(&str, &str)],
) -> (Answer, Answer) {
    let send = async |base: &str| {
        let mut request = client.request(method.clone(), format!("{base}{path}"));
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));

        let status = response.status().as_u16();
        let mut kept: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter(|(name, _)| !VOLATILE_HEADERS.contains(&name.as_str()))
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or("<binary>").to_owned(),
                )
            })
            .collect();
        kept.sort();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(std::borrow::ToOwned::to_owned);
        let body = response.bytes().await.expect("body reads").to_vec();
        Answer {
            status,
            headers: kept,
            body,
            served_by,
        }
    };

    (send(GO).await, send(RUST).await)
}

#[derive(Debug, PartialEq, Eq)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// `x-mmrs-served-by`, kept out of the compared headers and asserted separately — a
    /// comparison against a *forwarded* response proves nothing about the Rust handler.
    served_by: Option<String>,
}

/// Assert the two answers are identical **and that Rust answered locally**.
///
/// The served-by check is not decoration: every handler here forwards to Go on some branch, and
/// a forwarded answer compares equal to Go's by construction. Without this a route could stop
/// being served entirely and the suite would stay green.
async fn assert_identical(
    client: &reqwest::Client,
    token: Option<&str>,
    method: reqwest::Method,
    path: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let (go, rust) = fetch_both_full(client, token, method.clone(), path, headers).await;

    assert_eq!(
        go.status, rust.status,
        "{method} {path} {headers:?}: status"
    );
    assert_eq!(
        go.headers, rust.headers,
        "{method} {path} {headers:?}: headers"
    );
    assert_eq!(go.body, rust.body, "{method} {path} {headers:?}: body");
    assert_eq!(
        rust.served_by.as_deref(),
        Some("rust"),
        "{method} {path} {headers:?} was forwarded, so this comparison proves nothing"
    );
    rust
}

/// [`assert_identical`] without the served-by check, for a case this port deliberately forwards.
async fn assert_identical_allowing_forward(
    client: &reqwest::Client,
    token: Option<&str>,
    method: reqwest::Method,
    path: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let (go, rust) = fetch_both_full(client, token, method.clone(), path, headers).await;
    assert_eq!(go.status, rust.status, "{method} {path}: status");
    assert_eq!(go.headers, rust.headers, "{method} {path}: headers");
    assert_eq!(go.body, rust.body, "{method} {path}: body");
    rust
}

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// 45 bytes of text, long enough that a middle range names a slice a copy could get wrong and
/// short enough to compare by eye in a failure.
const TEXT_BODY: &[u8] = b"mmrs parity: abcdefghijklmnopqrstuvwxyz 01234";

struct Fixture {
    /// A PNG, attached to a post — so it has a thumbnail and a preview.
    png_id: String,
    /// A text file, attached — so it has *neither*, which is the 400 branch.
    text_id: String,
    /// A team whose icon has never been uploaded.
    team_id: String,
    /// The admin's own user id.
    user_id: String,
    /// A live custom emoji, whose image is the same PNG.
    emoji_id: String,
    emoji_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let user_id = logged_in_user_id().to_owned();
            let team_id = create_team(client, token, "fbteam").await;
            let channel_id = create_channel(client, token, &team_id, "fbchan").await;

            let png_id = upload_file(
                client,
                token,
                &channel_id,
                "mmrs-parity.png",
                "image/png",
                TINY_PNG,
            )
            .await;
            let text_id = upload_file(
                client,
                token,
                &channel_id,
                "mmrs-parity.txt",
                "text/plain",
                TEXT_BODY,
            )
            .await;

            // Attaching is what fills `PostId`; the bytes and the derived images are written at
            // upload time either way.
            post_message_with_files(
                client,
                token,
                &channel_id,
                "file bytes fixture",
                &[png_id.clone(), text_id.clone()],
            )
            .await;

            let emoji_name = unique_emoji_name("fbemoji");
            let emoji_id = create_custom_emoji(client, token, &user_id, &emoji_name).await;

            Fixture {
                png_id,
                text_id,
                team_id,
                user_id,
                emoji_id,
                emoji_name,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// GET /api/v4/files/{file_id}
// ---------------------------------------------------------------------------------------------

/// The original bytes, for an image served `inline` and a text file served as an `attachment` —
/// the two sides of `setHeaders`' media-type test.
#[tokio::test]
async fn the_original_bytes_and_every_header_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let png = assert_identical(
        &client,
        Some(&token),
        reqwest::Method::GET,
        &format!("/api/v4/files/{}", f.png_id),
        &[],
    )
    .await;
    assert_eq!(png.body, TINY_PNG, "the bytes are the ones uploaded");

    let text = assert_identical(
        &client,
        Some(&token),
        reqwest::Method::GET,
        &format!("/api/v4/files/{}", f.text_id),
        &[],
    )
    .await;
    assert_eq!(text.body, TEXT_BODY);

    // The two dispositions, asserted rather than merely compared: if both servers regressed the
    // same way the comparison above would still pass.
    let disposition = |answer: &Answer| {
        answer
            .headers
            .iter()
            .find(|(name, _)| name == "content-disposition")
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert!(
        disposition(&png).starts_with("inline;"),
        "image/png is a media type: {}",
        disposition(&png)
    );
    assert!(
        disposition(&text).starts_with("attachment;"),
        "text/plain is not: {}",
        disposition(&text)
    );
}

/// `?download=true` turns the inline image into an attachment; `?download=yes` does **not**,
/// because `strconv.ParseBool` rejects it and the handler discards the error.
#[tokio::test]
async fn the_download_flag_is_parse_bool_not_a_string_compare() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (query, want_attachment) in [
        ("?download=true", true),
        ("?download=1", true),
        ("?download=t", true),
        ("?download=yes", false),
        ("?download=", false),
        ("", false),
    ] {
        let answer = assert_identical(
            &client,
            Some(&token),
            reqwest::Method::GET,
            &format!("/api/v4/files/{}{query}", f.png_id),
            &[],
        )
        .await;
        let disposition = answer
            .headers
            .iter()
            .find(|(name, _)| name == "content-disposition")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(
            disposition.starts_with("attachment;"),
            want_attachment,
            "?{query} → {disposition}"
        );
    }
}

/// `HEAD` carries the same headers as `GET`, including `Content-Length`, and an empty body.
#[tokio::test]
async fn head_carries_the_headers_and_no_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let answer = assert_identical(
        &client,
        Some(&token),
        reqwest::Method::HEAD,
        &format!("/api/v4/files/{}", f.text_id),
        &[],
    )
    .await;
    assert!(answer.body.is_empty());
    assert!(
        answer
            .headers
            .iter()
            .any(|(name, value)| name == "content-length" && value == "45"),
        "a HEAD still declares the length: {:?}",
        answer.headers
    );
}

/// The whole `Range` family, including the two 416s — which differ from each other by one header.
#[tokio::test]
async fn range_requests_match_including_both_416s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/files/{}", f.text_id);

    for range in [
        "bytes=0-4",
        "bytes=10-19",
        "bytes=40-",
        "bytes=-5",
        "bytes=40-9999",
        // Exactly at the end (the file is 45 bytes): `start >= size` does not overlap, so this
        // is a 416 and not an empty 206. One byte earlier is a one-byte 206.
        "bytes=45-",
        "bytes=45-50",
        "bytes=44-",
        // Every range past the end: `errNoOverlap`, which carries a `Content-Range`.
        "bytes=1000-2000",
        // Not `bytes=`: `invalid range`, which does not.
        "chunks=1-2",
        "bytes=5-1",
        "bytes=-",
        // A satisfiable range beside an unsatisfiable one is a 206 for the satisfiable half.
        "bytes=0-4,1000-2000",
        // Sums to more than the file: the ranges are ignored and the whole file is sent.
        "bytes=0-40,0-40",
    ] {
        assert_identical(
            &client,
            Some(&token),
            reqwest::Method::GET,
            &path,
            &[("Range", range)],
        )
        .await;
    }
}

/// `If-Modified-Since`, `If-None-Match`, `If-Match`, `If-Unmodified-Since` and `If-Range`, whose
/// answers are 304, 412 and 200 and are defined by the headers they drop.
#[tokio::test]
async fn conditional_requests_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/files/{}", f.text_id);

    // Read the file's own `Last-Modified` back and condition on it, so the dates are the row's
    // rather than invented — a date the server does not recognise would make every case a 200
    // and the suite would prove nothing.
    let baseline = assert_identical(&client, Some(&token), reqwest::Method::GET, &path, &[]).await;
    let last_modified = baseline
        .headers
        .iter()
        .find(|(name, _)| name == "last-modified")
        .map(|(_, value)| value.clone())
        .expect("a file has a Last-Modified");
    let stale = "Thu, 04 Mar 2021 05:06:07 GMT";

    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![("If-Modified-Since", last_modified.as_str())],
        vec![("If-Modified-Since", stale)],
        vec![("If-Modified-Since", "not a date")],
        vec![("If-None-Match", "*")],
        vec![("If-None-Match", "\"anything\"")],
        vec![("If-Match", "*")],
        vec![("If-Match", "\"anything\"")],
        vec![("If-Unmodified-Since", stale)],
        vec![("If-Unmodified-Since", last_modified.as_str())],
        vec![("Range", "bytes=0-4"), ("If-Range", last_modified.as_str())],
        vec![("Range", "bytes=0-4"), ("If-Range", stale)],
        vec![("Range", "bytes=0-4"), ("If-Range", "\"anetag\"")],
    ];
    for headers in cases {
        assert_identical(&client, Some(&token), reqwest::Method::GET, &path, &headers).await;
    }
}

// ---------------------------------------------------------------------------------------------
// thumbnail and preview
// ---------------------------------------------------------------------------------------------

/// A PNG has both derived images; a text file has neither, and asking for one is a **400** whose
/// `detailed_error` carries the file id.
#[tokio::test]
async fn the_derived_images_match_and_a_text_file_has_none() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for suffix in ["thumbnail", "preview"] {
        let image = assert_identical(
            &client,
            Some(&token),
            reqwest::Method::GET,
            &format!("/api/v4/files/{}/{suffix}", f.png_id),
            &[],
        )
        .await;
        assert_eq!(image.status, 200);
        assert!(
            image
                .headers
                .iter()
                .any(|(name, value)| name == "content-type" && value == "image/jpeg"),
            "both derived images claim image/jpeg whatever the original was: {:?}",
            image.headers
        );

        let (go, rust) = fetch_both_raw(
            &client,
            &token,
            &format!("/api/v4/files/{}/{suffix}", f.text_id),
        )
        .await;
        assert_eq!(go.0, 400, "a text file has no {suffix}");
        assert_eq!(rust.0, 400);
        assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, suffix);
    }
}

// ---------------------------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------------------------

/// A file id that does not exist, on all three byte routes plus the info route — three different
/// error ids for the same situation.
#[tokio::test]
async fn a_missing_file_matches_on_every_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let absent = "mmrsfilebytes000000000000a";

    for path in [
        format!("/api/v4/files/{absent}"),
        format!("/api/v4/files/{absent}/thumbnail"),
        format!("/api/v4/files/{absent}/preview"),
        format!("/api/v4/files/{absent}/info"),
    ] {
        let (go, rust) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go.0, rust.0, "{path}: status");
        assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, &path);
    }
}

/// A malformed id — the wrong length, and outside gorilla's `[A-Za-z0-9]+` charset.
#[tokio::test]
async fn malformed_file_ids_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // Alphanumeric but the wrong **length** — gorilla routes these (its class is
    // `[A-Za-z0-9]+` with no length bound) and `RequireFileId` refuses them, so they are ours to
    // answer. An id carrying `-` or `_` is outside the class, never reaches a Go handler, and is
    // forwarded by `mux_segments_or_forward` — covered by that middleware's own tests.
    for id in ["short", "waytoolongtobeamattermostidatall", "0"] {
        for suffix in ["", "/thumbnail", "/preview"] {
            let path = format!("/api/v4/files/{id}{suffix}");
            let (go, rust) = fetch_both_raw(&client, &token, &path).await;
            assert_eq!(go.0, rust.0, "{path}: status");
            assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, &path);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the four image routes
// ---------------------------------------------------------------------------------------------

/// The profile image, its etag, and the 304 the etag buys.
#[tokio::test]
async fn the_profile_image_and_its_etag_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/users/{}/image", f.user_id);

    let answer =
        assert_identical_allowing_forward(&client, Some(&token), reqwest::Method::GET, &path, &[])
            .await;
    if answer.served_by.as_deref() != Some("rust") {
        // The generated-avatar branch — a user whose `profile.png` is not in the backend. This
        // port forwards it (see `App::get_profile_image`), and the comparison above has already
        // shown the two servers agree. Nothing further to assert.
        return;
    }

    let etag = answer
        .headers
        .iter()
        .find(|(name, _)| name == "etag")
        .map(|(_, value)| value.clone())
        .expect("a served profile image carries its etag");
    // `strconv.FormatInt` — a bare integer, not a quoted entity tag.
    assert!(
        etag.parse::<i64>().is_ok(),
        "the etag is LastPictureUpdate rendered as an integer: {etag}"
    );

    assert_identical(
        &client,
        Some(&token),
        reqwest::Method::GET,
        &path,
        &[("If-None-Match", etag.as_str())],
    )
    .await;
}

/// A team with no icon — the 404 both servers give, and the `read_file` error id behind it.
#[tokio::test]
async fn a_team_without_an_icon_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rust) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/teams/{}/image", f.team_id),
    )
    .await;
    assert_eq!(go.0, rust.0, "status");
    assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "team icon");
}

/// A custom emoji's image, whose `Content-Type` comes from decoding the bytes.
#[tokio::test]
async fn the_emoji_image_matches_and_names_the_decoded_format() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let answer = assert_identical(
        &client,
        Some(&token),
        reqwest::Method::GET,
        &format!("/api/v4/emoji/{}/image", f.emoji_id),
        &[],
    )
    .await;
    assert_eq!(answer.status, 200);
    assert!(
        answer
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "image/png"),
        "the type is `image/` plus what image.DecodeConfig named: {:?}",
        answer.headers
    );
    let _ = &f.emoji_name;
}

/// The brand image, which no installation here has uploaded: a bare 404 with an **empty body**,
/// no error document at all — and `Content-Type: application/json`, which is Go's global default
/// surviving a handler that never sets one.
#[tokio::test]
async fn the_missing_brand_image_is_an_empty_404() {
    if !stack_enabled() {
        return;
    }
    // `image_writes` uploads a brand image and deletes it again; a read taken inside that window
    // is a 200 that this test would report as a route regression. See `common::BRAND_IMAGE`.
    let _guard = common::BRAND_IMAGE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let answer = assert_identical(
        &client,
        Some(&token),
        reqwest::Method::GET,
        "/api/v4/brand/image",
        &[],
    )
    .await;
    if answer.status == 404 {
        assert!(
            answer.body.is_empty(),
            "getBrandImage writes `nil`, not an AppError: {:?}",
            String::from_utf8_lossy(&answer.body)
        );
    }
}

/// The brand GET is unauthenticated — `APIHandlerTrustRequester` — and answers the same way with
/// no token at all.
#[tokio::test]
async fn the_brand_image_needs_no_session() {
    if !stack_enabled() {
        return;
    }
    let _guard = common::BRAND_IMAGE.lock().await;
    let client = client();
    // Ensure the fixture's purge has run before an unauthenticated request races it.
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    assert_identical(
        &client,
        None,
        reqwest::Method::GET,
        "/api/v4/brand/image",
        &[],
    )
    .await;
}
