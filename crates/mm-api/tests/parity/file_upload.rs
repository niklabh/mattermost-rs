//! Cross-server parity for `POST /api/v4/files` (`uploadFileStream`) — the classic simple-body
//! and multipart uploads.
//!
//! ```sh
//! scripts/parity.sh --test parity file_upload
//! ```
//!
//! A text (non-image) upload is served here end to end and compared to Go; a raster image is
//! handed to Go before the write ([D-380]/[D-411]), so its assertion is that the request forwards
//! and Go's own `has_preview_image`/`width`/`height` come back. Both servers share one file-store
//! directory, and every upload targets an `mmrs-parity-` channel the purge collects.

use crate::common;

use common::{
    GO, RUST, TINY_PNG, a_team_and_channel_the_user_is_in, client, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

async fn send(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(std::borrow::ToOwned::to_owned);
    let body = response.bytes().await.expect("body reads").to_vec();
    (status, served_by, body)
}

/// Strip the per-upload fields from every `FileInfo` in a `FileUploadResponse`.
fn normalise(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(infos) = value.get_mut("file_infos").and_then(|v| v.as_array_mut()) {
        for info in infos {
            if let Some(obj) = info.as_object_mut() {
                for key in ["id", "create_at", "update_at", "post_id", "channel_id"] {
                    obj.remove(key);
                }
            }
        }
    }
    value
}

/// A multipart body with a `channel_id` field and one text file under `files`.
fn multipart_text(channel_id: &str, filename: &str, content: &[u8]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "mmrsparityfileboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"channel_id\"\r\n\r\n{channel_id}\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// A simple-body text upload is served locally, matches Go once normalised, and reads back.
#[tokio::test]
async fn a_simple_text_upload_matches_go_and_reads_back() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let content = b"mmrs parity POST /files simple body\n";
    let path = format!("/api/v4/files?channel_id={channel}&filename=mmrs-parity-simple.txt");

    let mut responses = Vec::new();
    for base in [GO, RUST] {
        let (status, served_by, body) =
            send(&client, base, &path, &token, "text/plain", content.to_vec()).await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        let value: serde_json::Value = serde_json::from_slice(&body).expect("a FileUploadResponse");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
            assert_eq!(
                value["file_infos"][0]["mime_type"],
                "text/plain; charset=utf-8"
            );
        }
        // Read the bytes back from the same server.
        let file_id = value["file_infos"][0]["id"]
            .as_str()
            .expect("a file id")
            .to_owned();
        let got = client
            .get(format!("{base}/api/v4/files/{file_id}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("a response")
            .bytes()
            .await
            .expect("bytes")
            .to_vec();
        assert_eq!(got, content, "{base} round-tripped bytes");
        responses.push(value);
    }
    assert_eq!(
        normalise(responses[0].clone()),
        normalise(responses[1].clone()),
        "the two FileUploadResponses differ once per-upload fields are normalised"
    );
}

/// A simple upload with a `client_id` echoes it back in `client_ids`.
#[tokio::test]
async fn a_client_id_is_echoed_back() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let path = format!(
        "/api/v4/files?channel_id={channel}&filename=mmrs-parity-cid.txt&client_id=mmrs-client-7"
    );
    for base in [GO, RUST] {
        let (status, served_by, body) = send(
            &client,
            base,
            &path,
            &token,
            "text/plain",
            b"body\n".to_vec(),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value["client_ids"][0], "mmrs-client-7",
            "{base} echoed client id"
        );
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
        }
    }
}

/// A multipart text upload is served locally and matches Go.
#[tokio::test]
async fn a_multipart_text_upload_matches_go() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let content = b"mmrs parity POST /files multipart\n";
    let (content_type, body) = multipart_text(&channel, "mmrs-parity-mp.txt", content);

    let mut responses = Vec::new();
    for base in [GO, RUST] {
        let (status, served_by, resp) = send(
            &client,
            base,
            "/api/v4/files",
            &token,
            &content_type,
            body.clone(),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&resp));
        let value: serde_json::Value = serde_json::from_slice(&resp).expect("a FileUploadResponse");
        assert_eq!(
            value["file_infos"][0]["name"], "mmrs-parity-mp.txt",
            "{base} name"
        );
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
        }
        responses.push(value);
    }
    assert_eq!(
        normalise(responses[0].clone()),
        normalise(responses[1].clone()),
        "multipart responses differ once normalised"
    );
}

/// A PNG is handed to Go, which fills in `has_preview_image`, `width` and `height` — the pixel
/// work this port defers. The assertion is that the request forwards and Go's dimensions come
/// back.
#[tokio::test]
async fn an_image_upload_forwards_and_go_measures_it() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let path = format!("/api/v4/files?channel_id={channel}&filename=mmrs-parity-pixel.png");
    let (status, served_by, body) =
        send(&client, RUST, &path, &token, "image/png", TINY_PNG.to_vec()).await;
    assert_eq!(
        status,
        201,
        "image upload: {}",
        String::from_utf8_lossy(&body)
    );
    assert_ne!(
        served_by.as_deref(),
        Some("rust"),
        "an image upload must forward to Go, not be served here"
    );
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let info = &value["file_infos"][0];
    assert_eq!(info["width"], 1, "Go measured the width");
    assert_eq!(info["height"], 1, "Go measured the height");
    assert_eq!(info["has_preview_image"], true, "Go set has_preview_image");
}

/// A simple upload with no `channel_id` is the 400 invalid-url-param on both servers.
#[tokio::test]
async fn a_missing_channel_id_is_rejected() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for base in [GO, RUST] {
        let (status, served_by, body) = send(
            &client,
            base,
            "/api/v4/files?filename=mmrs-parity-x.txt",
            &token,
            "text/plain",
            b"body\n".to_vec(),
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                value["id"], "api.context.invalid_url_param.app_error",
                "the id"
            );
        }
    }
}

/// With attachments off, a second mm-api answers **403** (not the resumable route's 501).
#[tokio::test]
async fn disabled_attachments_refuses_with_403() {
    if !stack_enabled() {
        return;
    }
    let Some(server) =
        common::SecondServer::start(8088, &[("MM_FILESETTINGS_ENABLEFILEATTACHMENTS", "false")])
            .await
    else {
        return;
    };
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let path = format!("/api/v4/files?channel_id={channel}&filename=x.txt");
    let (status, served_by, body) = send(
        &client,
        &server.base,
        &path,
        &token,
        "text/plain",
        b"body\n".to_vec(),
    )
    .await;
    assert_eq!(status, 403, "disabled: {}", String::from_utf8_lossy(&body));
    assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value["id"], "api.file.attachments.disabled.app_error",
        "the id"
    );
}
