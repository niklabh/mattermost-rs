//! Cross-server parity for `POST /api/v4/emoji` and `DELETE /api/v4/emoji/{emoji_id}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::emoji_writes
//! ```
//!
//! # A write cannot be sent to both servers and compared
//!
//! Creating the same emoji twice is a duplicate on the second attempt, and the row that comes back
//! carries a minted id and two clock readings. So the successes here are compared **shape for
//! shape** with those three fields masked, each against its own server, and the *refusals* — which
//! are pure functions of the request — are compared byte for byte.
//!
//! # What this server answers and what it hands over
//!
//! Everything up to and including the 1028×1028 refusal is answered from Rust. Past that, a
//! filename that is not `.png`, a format whose header the port does not measure, and any image
//! that needs resizing are forwarded — see `mm_app::App::upload_emoji_image` and [D-380]. Each
//! forward happens *before* the file backend is touched, so a forwarded create is Go's alone.
//! [`a_resize_and_a_gif_filename_are_answered_by_go`] measures the boundary.
//!
//! # Shared state, and the prefix that keeps it apart
//!
//! The emoji table is one table for every suite in this binary, and `emoji_list` asserts on list
//! membership while this one creates and deletes ([D-352]). Every name here is
//! `mmrsparitywrite<tag><millis>`, so nothing this suite writes can be mistaken for another
//! suite's fixture — and nothing is asserted about the table as a whole.

use crate::common;

use common::{
    GO, RUST, TINY_PNG, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled, unique_emoji_name,
};

/// `mmrsparitywrite<tag><millis>` — inside the emoji-name charset and outside every other suite's
/// prefix. See the module docs.
fn write_name(tag: &str) -> String {
    unique_emoji_name(&format!("write{tag}"))
}

/// One multipart body, assembled by hand for the same reason `common::create_custom_emoji` does.
///
/// `emoji_json` and the image part are both optional, because their *absence* is two of the
/// refusals under test.
fn multipart_body(
    boundary: &str,
    emoji_json: Option<&str>,
    image: Option<(&str, &[u8])>,
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    if let Some((filename, bytes)) = image {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; \
                 filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    if let Some(json) = emoji_json {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"emoji\"\r\n\r\n{json}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// `(status, body, x-mmrs-served-by)` for one `POST /api/v4/emoji`.
async fn post_emoji(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .post(format!("{base}/api/v4/emoji"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} POST /api/v4/emoji is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// The ordinary shape: a 1×1 PNG named `e.png`, with the emoji JSON beside it.
async fn post_plain_emoji(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
) -> (u16, Vec<u8>, Option<String>) {
    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let creator = logged_in_user_id();
    let json = format!(r#"{{"name":"{name}","creator_id":"{creator}"}}"#);
    let body = multipart_body(BOUNDARY, Some(&json), Some(("e.png", TINY_PNG)));
    post_emoji(
        client,
        base,
        token,
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        body,
    )
    .await
}

/// A PNG that declares `width`×`height` in a valid IHDR and stops there.
///
/// `image.DecodeConfig` returns as soon as IHDR is read for a non-paletted image, so this is a
/// decodable image of any size for the purposes of `uploadEmojiImage` without carrying the
/// megabytes a real one would — and the 1028 refusal is reached before anything tries to decode
/// the pixels.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffff_u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        out
    }

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // 8-bit truecolour with alpha, no compression/filter/interlace variation.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&chunk(b"IEND", b""));
    png
}

/// Strip the three fields that cannot match across two independent creates.
fn masked(body: &[u8], context: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: the body is not JSON: {e}\n{}",
            String::from_utf8_lossy(body)
        )
    });
    let object = value
        .as_object_mut()
        .unwrap_or_else(|| panic!("{context}: the body is not an object"));
    for key in ["id", "create_at", "update_at", "name"] {
        assert!(object.contains_key(key), "{context}: `{key}` is missing");
        object.insert(key.to_owned(), serde_json::Value::Null);
    }
    value
}

// ---------------------------------------------------------------------------------------------
// createEmoji — the success
// ---------------------------------------------------------------------------------------------

/// Both servers write a row that reads back identically through the already-migrated `GET`.
///
/// The **trailing newline** is asserted too: `createEmoji` writes with `json.NewEncoder`, unlike
/// the `ReturnStatusOK` bodies beside it.
#[tokio::test]
async fn a_create_writes_the_same_row_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;

    let go_name = write_name("okgo");
    let rs_name = write_name("okrs");

    let (go_status, go_body, _) = post_plain_emoji(&client, GO, &token, &go_name).await;
    let (rs_status, rs_body, served_by) = post_plain_emoji(&client, RUST, &token, &rs_name).await;

    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        200,
        "rust: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(served_by.as_deref(), Some("rust"), "a plain PNG is ours");

    assert!(
        go_body.ends_with(b"\n") && rs_body.ends_with(b"\n"),
        "createEmoji is encoder-framed on both servers"
    );
    assert_eq!(
        masked(&go_body, "go"),
        masked(&rs_body, "rust"),
        "every field but the id, the timestamps and the name must match"
    );

    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(rs["name"], serde_json::Value::String(rs_name.clone()));
    assert_eq!(
        rs["id"].as_str().map(str::len),
        Some(26),
        "the id is minted, and it is an id"
    );
    assert_eq!(rs["delete_at"], serde_json::Value::from(0));
    assert_eq!(
        rs["creator_id"],
        serde_json::Value::String(logged_in_user_id().to_owned())
    );
    assert_eq!(
        rs["create_at"], rs["update_at"],
        "PreSave copies one onto the other"
    );

    // **A caller cannot choose the id, or overwrite an existing emoji with it.** `CreateEmoji`
    // blanks `emoji.Id` before `PreSave` (app/emoji.go:51), so a supplied id is ignored rather
    // than honoured — and it is ignored on both servers.
    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    let supplied = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    for (server, tag) in [(GO, "idgo"), (RUST, "idrs")] {
        let name = write_name(tag);
        let body = multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"id":"{supplied}","name":"{name}","creator_id":"{}"}}"#,
                logged_in_user_id()
            )),
            Some(("e.png", TINY_PNG)),
        );
        let (status, body, _) = post_emoji(&client, server, &token, &content_type, body).await;
        assert_eq!(status, 200, "{server}: {}", String::from_utf8_lossy(&body));
        let created: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_ne!(
            created["id"], supplied,
            "{server}: the supplied id must be wiped, not honoured"
        );
    }

    // The row is real on both servers: read each back through the migrated GET.
    for (server, body) in [(GO, &go_body), (RUST, &rs_body)] {
        let created: serde_json::Value = serde_json::from_slice(body).expect("JSON");
        let id = created["id"].as_str().expect("an id");
        let read = client
            .get(format!("{server}/api/v4/emoji/{id}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server answers")
            .json::<serde_json::Value>()
            .await
            .expect("the emoji decodes");
        assert_eq!(&read, &created, "{server}: the row reads back unchanged");
    }

    // And the image really landed: `GET /emoji/{id}/image` serves the bytes that were posted.
    let rs_created: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    let id = rs_created["id"].as_str().expect("an id");
    let image = client
        .get(format!("{GO}/api/v4/emoji/{id}/image"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        image.status(),
        200,
        "Go must be able to read back the image this server wrote"
    );
    assert_eq!(
        image.bytes().await.expect("bytes").to_vec(),
        TINY_PNG.to_vec(),
        "a small PNG is written through untouched, byte for byte"
    );
}

// ---------------------------------------------------------------------------------------------
// createEmoji — the refusals
// ---------------------------------------------------------------------------------------------

/// Every refusal that is a pure function of the request, compared byte for byte.
///
/// They are run in one test because each is two HTTP round trips and the suite's cost is round
/// trips; the `context` in the assertion says which row failed.
#[tokio::test]
async fn the_create_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let creator = logged_in_user_id();
    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let multipart = format!("multipart/form-data; boundary={BOUNDARY}");

    // Each row is (context, content-type, body).
    let mut cases: Vec<(String, String, Vec<u8>)> = Vec::new();

    // No `emoji` value at all → `invalid_body_param` naming `emoji`.
    cases.push((
        "no emoji part".to_owned(),
        multipart.clone(),
        multipart_body(BOUNDARY, None, Some(("e.png", TINY_PNG))),
    ));
    // An `emoji` value that is not JSON → the *same* 400, indistinguishable from the above.
    cases.push((
        "emoji part is not json".to_owned(),
        multipart.clone(),
        multipart_body(BOUNDARY, Some("{"), Some(("e.png", TINY_PNG))),
    ));
    // A name the model refuses: a system emoji name, and a malformed one. Different error ids.
    cases.push((
        "system emoji name".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"grinning","creator_id":"{creator}"}}"#
            )),
            Some(("e.png", TINY_PNG)),
        ),
    ));
    cases.push((
        "name outside the charset".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"has space","creator_id":"{creator}"}}"#
            )),
            Some(("e.png", TINY_PNG)),
        ),
    ));
    cases.push((
        "empty name".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(r#"{{"name":"","creator_id":"{creator}"}}"#)),
            Some(("e.png", TINY_PNG)),
        ),
    ));
    // A creator that is not the session's user → 403, and it is checked *after* validation.
    cases.push((
        "someone else's creator_id".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"{}","creator_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz"}}"#,
                write_name("other")
            )),
            Some(("e.png", TINY_PNG)),
        ),
    ));
    // No image part. Reached only *after* the name is validated and the duplicate check passes,
    // so the name here has to be a good one.
    cases.push((
        "no image part".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"{}","creator_id":"{creator}"}}"#,
                write_name("noimg")
            )),
            None,
        ),
    ));
    // Bytes that are not an image at all → `api.emoji.upload.image.app_error`.
    cases.push((
        "the image is not an image".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"{}","creator_id":"{creator}"}}"#,
                write_name("notimg")
            )),
            Some(("e.png", b"not an image at all")),
        ),
    ));
    // Over 1028 in one dimension → `large_image.too_large`, with MaxWidth and MaxHeight.
    cases.push((
        "wider than 1028".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"{}","creator_id":"{creator}"}}"#,
                write_name("wide")
            )),
            Some(("e.png", &png_header(1029, 8))),
        ),
    ));
    cases.push((
        "taller than 1028".to_owned(),
        multipart.clone(),
        multipart_body(
            BOUNDARY,
            Some(&format!(
                r#"{{"name":"{}","creator_id":"{creator}"}}"#,
                write_name("tall")
            )),
            Some(("e.png", &png_header(8, 1029))),
        ),
    ));
    // Not multipart at all, and multipart with no boundary → the parse 400.
    cases.push((
        "a JSON body".to_owned(),
        "application/json".to_owned(),
        br#"{"name":"x"}"#.to_vec(),
    ));
    cases.push((
        "multipart with no boundary".to_owned(),
        "multipart/form-data".to_owned(),
        multipart_body(BOUNDARY, Some("{}"), None),
    ));
    cases.push((
        "a body that is not the declared multipart".to_owned(),
        multipart.clone(),
        b"nothing that looks like a part".to_vec(),
    ));

    for (context, content_type, body) in cases {
        let (go_status, go_body, _) =
            post_emoji(&client, GO, &token, &content_type, body.clone()).await;
        let (rs_status, rs_body, served_by) =
            post_emoji(&client, RUST, &token, &content_type, body).await;

        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{context}: this refusal is ours to give"
        );
        assert_eq!(
            rs_status,
            go_status,
            "{context}: status\n  go:   {}\n  rust: {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
    }
}

/// The duplicate check, which needs a row to collide with.
///
/// It sits **after** validation and after the `creator_id` check, so an invalid name that is also
/// taken is the validation error — the ordering is what this pins, not merely the id.
#[tokio::test]
async fn a_duplicate_name_is_refused_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;

    let taken = write_name("dup");
    create_custom_emoji(&client, &token, logged_in_user_id(), &taken).await;

    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let creator = logged_in_user_id();
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(r#"{{"name":"{taken}","creator_id":"{creator}"}}"#)),
        Some(("e.png", TINY_PNG)),
    );
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    let (go_status, go_body, _) =
        post_emoji(&client, GO, &token, &content_type, body.clone()).await;
    let (rs_status, rs_body, served_by) =
        post_emoji(&client, RUST, &token, &content_type, body).await;

    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "duplicate name");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(rs["id"], "api.emoji.create.duplicate.app_error");

    // A name that is taken *and* invalid is the validation error, not the duplicate one.
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"grinning","creator_id":"{creator}"}}"#
        )),
        Some(("e.png", TINY_PNG)),
    );
    let (_, rs_body, _) = post_emoji(&client, RUST, &token, &content_type, body).await;
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(
        rs["id"], "model.emoji.system_emoji_name.app_error",
        "validation precedes the duplicate check"
    );
}

/// A declared `Content-Length` over 512 KiB is a **413**, before the body is read at all.
#[tokio::test]
async fn an_oversized_upload_is_a_413() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let creator = logged_in_user_id();
    // 512 KiB + 1 of payload, so the framed body is comfortably over the cap and its
    // `Content-Length` is too. reqwest sets the header from the body it is given.
    let oversized = vec![b'x'; (1 << 19) + 1];
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"{}","creator_id":"{creator}"}}"#,
            write_name("big")
        )),
        Some(("e.png", &oversized)),
    );
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    let (go_status, go_body, _) =
        post_emoji(&client, GO, &token, &content_type, body.clone()).await;
    let (rs_status, rs_body, served_by) =
        post_emoji(&client, RUST, &token, &content_type, body).await;

    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 413, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        413,
        "rust: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "oversized upload");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(rs["id"], "api.emoji.create.too_large.app_error");
}

/// The forwarding boundary: a filename that is not `.png`, and an image that needs resizing.
///
/// Both are answered by **Go**, and both still work — a forward is not a degradation, it is the
/// strangler doing its job. The assertion is on `x-mmrs-served-by`, because the body is Go's
/// either way and would tell us nothing about which server produced it.
#[tokio::test]
async fn a_resize_and_a_gif_filename_are_answered_by_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;

    const BOUNDARY: &str = "mmrsparitywriteboundary";
    let creator = logged_in_user_id();
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    // 1. A `.gif` filename. The bytes are a PNG, so Go's `CountGIFFrames` fails and answers 400 —
    //    the point is that the *decision* was Go's, not that the upload succeeded.
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"{}","creator_id":"{creator}"}}"#,
            write_name("giffn")
        )),
        Some(("e.gif", TINY_PNG)),
    );
    let (status, body, served_by) = post_emoji(&client, RUST, &token, &content_type, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "a non-.png filename is handed over"
    );
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let forwarded: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(forwarded["id"], "api.emoji.upload.image.app_error");

    // 2. An image between 128 and 1028: the resize path, whose output bytes are Go's to produce.
    //    A header-only PNG is enough to get past `DecodeConfig` and reach the resize, where Go
    //    then fails to decode the (absent) pixels — again, the assertion is who decided.
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"{}","creator_id":"{creator}"}}"#,
            write_name("resize")
        )),
        Some(("e.png", &png_header(200, 200))),
    );
    let (_, body, served_by) = post_emoji(&client, RUST, &token, &content_type, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "an image needing a resize is handed over"
    );
    // Whatever Go makes of it, it is Go's answer and not a 502 from a broken forward.
    let forwarded: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert!(
        forwarded.get("id").is_some(),
        "the forward produced a real AppError: {}",
        String::from_utf8_lossy(&body)
    );

    // 3. Exactly 128 is **not** a resize — the check is `>`, not `>=` — so this one is ours.
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"{}","creator_id":"{creator}"}}"#,
            write_name("edge")
        )),
        Some(("e.png", &png_header(128, 128))),
    );
    let (_, _, served_by) = post_emoji(&client, RUST, &token, &content_type, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "128 x 128 is the write-through path"
    );

    // 4. And exactly 1028 is **not** too large, for the same reason — so it reaches the resize
    //    and is handed over rather than refused here. Without this row an off-by-one in the
    //    refusal threshold is invisible: 1029 is refused either way.
    let body = multipart_body(
        BOUNDARY,
        Some(&format!(
            r#"{{"name":"{}","creator_id":"{creator}"}}"#,
            write_name("atlimit")
        )),
        Some(("e.png", &png_header(1028, 1028))),
    );
    let (_, _, served_by) = post_emoji(&client, RUST, &token, &content_type, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "1028 x 1028 is inside the limit and needs resizing, so it is Go's"
    );
}

// ---------------------------------------------------------------------------------------------
// deleteEmoji
// ---------------------------------------------------------------------------------------------

/// `(status, body, x-mmrs-served-by)` for one `DELETE /api/v4/emoji/{id}`.
async fn delete_emoji(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    emoji_id: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .delete(format!("{base}/api/v4/emoji/{emoji_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} DELETE is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// The success body is `{"status":"OK"}` with **no** trailing newline, and the row is soft-deleted
/// on both servers identically.
#[tokio::test]
async fn a_delete_matches_go_and_leaves_the_row_behind() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let creator = logged_in_user_id();

    let go_id = create_custom_emoji(&client, &token, creator, &write_name("delgo")).await;
    let rs_id = create_custom_emoji(&client, &token, creator, &write_name("delrs")).await;

    let (go_status, go_body, _) = delete_emoji(&client, GO, &token, &go_id).await;
    let (rs_status, rs_body, served_by) = delete_emoji(&client, RUST, &token, &rs_id).await;

    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(go_body, rs_body, "the two status bodies are identical");
    assert_eq!(
        rs_body, br#"{"status":"OK"}"#,
        "ReturnStatusOK has no trailing newline"
    );

    // Both are gone from the reads, on both servers.
    for id in [&go_id, &rs_id] {
        for base in [GO, RUST] {
            let status = client
                .get(format!("{base}/api/v4/emoji/{id}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("the server answers")
                .status()
                .as_u16();
            assert_eq!(status, 404, "{base} still has {id}");
        }
    }

    // A second delete is a 404 with the same id and body on both servers.
    let (go_status, go_body, _) = delete_emoji(&client, GO, &token, &go_id).await;
    let (rs_status, rs_body, served_by) = delete_emoji(&client, RUST, &token, &rs_id).await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "deleting twice");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(
        rs["id"], "app.emoji.get.no_result",
        "the *fetch* fails first — the store's delete 404 is unreachable through the route"
    );
}

/// A malformed id and an unknown id, and the fact that the fetch precedes every permission check.
#[tokio::test]
async fn the_delete_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (context, id, expected) in [
        // `RequireEmojiId`: inside the mux charset, wrong length.
        ("a short id", "abc", 400),
        ("a long id", "abcdefghijklmnopqrstuvwxyz0", 400),
        // 26 characters, names nothing.
        ("an unknown id", "zzzzzzzzzzzzzzzzzzzzzzzzzz", 404),
    ] {
        let (go_status, go_body, _) = delete_emoji(&client, GO, &token, id).await;
        let (rs_status, rs_body, served_by) = delete_emoji(&client, RUST, &token, id).await;

        assert_eq!(served_by.as_deref(), Some("rust"), "{context}");
        assert_eq!(go_status, expected, "{context}: Go");
        assert_eq!(rs_status, expected, "{context}: rust");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
    }

    // A segment outside `[A-Za-z0-9]+` is a mux 404 on Go, which this server forwards rather than
    // reproducing — the same rule every id-shaped route here follows.
    let (_, _, served_by) = delete_emoji(&client, RUST, &token, "not-an-id").await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "a segment gorilla would not have routed is Go's own 404 to give"
    );
}

/// Deleting an emoji sweeps the reactions that used it, which is the part of `DeleteEmoji` no
/// status code reports.
#[tokio::test]
async fn deleting_an_emoji_removes_the_reactions_that_used_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let creator = logged_in_user_id();

    let name = write_name("react");
    let emoji_id = create_custom_emoji(&client, &token, creator, &name).await;

    let channel_id = common::a_channel_the_user_is_in(&client, &token).await;
    let post_id = common::post_message(
        &client,
        &token,
        &channel_id,
        "mmrs parity reaction post",
        None,
    )
    .await;

    let reaction = client
        .post(format!("{GO}/api/v4/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "user_id": creator,
            "post_id": post_id,
            "emoji_name": name,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        reaction.status().is_success(),
        "reacting failed: {}",
        reaction.text().await.unwrap_or_default()
    );

    // # Read the table, not the route
    //
    // `GET /posts/{id}/reactions` is served by Go out of `LocalCacheReactionStore`, which is
    // invalidated by **Go's own** `DeleteAllWithEmojiName` and by nothing else. This server writes
    // the same rows through the same database and Go never hears about it, so the route would
    // keep answering from a cache for as long as it holds the entry — a staleness difference
    // ([D-383]), not a missing write. The table is the oracle for that reason.
    let live_reactions = async || -> i64 {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set; scripts/parity.sh sets it");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connects to Postgres");
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM reactions
              WHERE emojiname = $1 AND COALESCE(deleteat, 0) = 0",
        )
        .bind(&name)
        .fetch_one(&pool)
        .await
        .expect("counts the live reactions")
    };
    assert_eq!(live_reactions().await, 1, "the reaction is there to remove");

    let (status, body, served_by) = delete_emoji(&client, RUST, &token, &emoji_id).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(served_by.as_deref(), Some("rust"));

    assert_eq!(
        live_reactions().await,
        0,
        "the reaction that used the deleted emoji is swept, as Go's deleteReactionsForEmoji does"
    );

    // And `Posts.HasReactions` was recomputed, which is the second statement of the sweep.
    let has_reactions = {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connects to Postgres");
        sqlx::query_scalar::<_, bool>("SELECT hasreactions FROM posts WHERE id = $1")
            .bind(&post_id)
            .fetch_one(&pool)
            .await
            .expect("reads the post")
    };
    assert!(
        !has_reactions,
        "the post's HasReactions is recomputed by the sweep"
    );
}
