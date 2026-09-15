//! Cross-server parity for the resumable upload writes: `POST /api/v4/uploads` (`createUpload`)
//! and `POST /api/v4/uploads/{upload_id}` (`uploadData`).
//!
//! ```sh
//! scripts/parity.sh --test parity upload_write
//! ```
//!
//! Both servers write into the **same** file-store directory (`scripts/parity.sh` exports one
//! `MM_FILESETTINGS_DIRECTORY` for mm-api and Go alike), under different file ids, so each
//! server's upload is read back from that same server. The session rows land in the shared
//! database and carry the channel's `mmrs-parity-` name, which `purge_api_fixtures` collects.

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, add_user_to_channel, client, create_channel,
    create_plain_user, create_team, delete_plain_user, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// One request to a base, returning `(status, x-mmrs-served-by, body)`.
#[allow(clippy::too_many_arguments)]
async fn send(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    content_length: Option<i64>,
    body: Vec<u8>,
) -> (u16, Option<String>, Vec<u8>) {
    let mut request = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .body(body);
    if let Some(ct) = content_type {
        request = request.header("Content-Type", ct);
    }
    if let Some(len) = content_length {
        request = request.header("Content-Length", len.to_string());
    }
    let response = request
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

/// Drop the fields two independent uploads cannot share.
fn normalise(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for key in ["id", "create_at", "update_at", "post_id", "channel_id"] {
            obj.remove(key);
        }
    }
    value
}

async fn create_session(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    filename: &str,
    file_size: i64,
) -> (u16, Option<String>, serde_json::Value) {
    let body = serde_json::json!({
        "channel_id": channel_id,
        "filename": filename,
        "file_size": file_size,
        "type": "attachment",
    });
    let (status, served_by, bytes) = send(
        client,
        base,
        reqwest::Method::POST,
        "/api/v4/uploads",
        token,
        Some("application/json"),
        None,
        serde_json::to_vec(&body).unwrap(),
    )
    .await;
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, served_by, value)
}

/// A small text file completes in one chunk on both servers, the `FileInfo` matches once the
/// per-upload fields are normalised, and the bytes are readable back from each.
#[tokio::test]
async fn a_text_upload_completes_identically_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let content = b"mmrs parity: the quick brown fox.\n";
    let mut infos = Vec::new();
    for base in [GO, RUST] {
        let (status, served_by, session) = create_session(
            &client,
            base,
            &token,
            &channel,
            "mmrs-parity-note.txt",
            content.len() as i64,
        )
        .await;
        assert_eq!(status, 201, "{base} createUpload: {session}");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "createUpload served by");
        }
        let upload_id = session["id"].as_str().expect("a session id").to_owned();

        let (status, served_by, body) = send(
            &client,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/uploads/{upload_id}"),
            &token,
            Some("application/octet-stream"),
            Some(content.len() as i64),
            content.to_vec(),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{base} uploadData status: {}",
            String::from_utf8_lossy(&body)
        );
        if base == RUST {
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "uploadData served by rust"
            );
        }
        let info: serde_json::Value = serde_json::from_slice(&body).expect("a FileInfo");
        assert_eq!(info["name"], "mmrs-parity-note.txt", "{base} name");
        assert_eq!(info["extension"], "txt", "{base} extension");
        assert_eq!(
            info["mime_type"], "text/plain; charset=utf-8",
            "{base} mime"
        );
        assert_eq!(info["size"], content.len() as i64, "{base} size");

        // The bytes landed and read back.
        let file_id = info["id"].as_str().expect("a file id").to_owned();
        let (get_status, _, got) = send(
            &client,
            base,
            reqwest::Method::GET,
            &format!("/api/v4/files/{file_id}"),
            &token,
            None,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(get_status, 200, "{base} GET /files/{file_id}");
        assert_eq!(got, content, "{base} round-tripped bytes");

        infos.push(info);
    }
    assert_eq!(
        normalise(infos[0].clone()),
        normalise(infos[1].clone()),
        "the two FileInfos differ once per-upload fields are normalised"
    );
}

/// A first chunk at least `minFirstPartSize` that does not finish the file is **204**, and the
/// completing chunk after it returns the `FileInfo`.
#[tokio::test]
async fn a_first_chunk_short_of_the_size_is_204_then_completes() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    // 5 MiB first chunk (the floor), then a tail.
    let first = vec![b'a'; 5 * 1024 * 1024];
    let tail = b"mmrs-tail".to_vec();
    let total = (first.len() + tail.len()) as i64;

    for base in [GO, RUST] {
        let (status, _, session) = create_session(
            &client,
            base,
            &token,
            &channel,
            "mmrs-parity-big.txt",
            total,
        )
        .await;
        assert_eq!(status, 201, "{base} createUpload: {session}");
        let upload_id = session["id"].as_str().unwrap().to_owned();

        // First chunk: incomplete → 204, no body.
        let (status, served_by, body) = send(
            &client,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/uploads/{upload_id}"),
            &token,
            Some("application/octet-stream"),
            Some(first.len() as i64),
            first.clone(),
        )
        .await;
        assert_eq!(
            status,
            204,
            "{base} first chunk: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(body.is_empty(), "{base} 204 has no body");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "204 served by rust");
        }

        // Completing chunk → 201/200 with the FileInfo.
        let (status, _, body) = send(
            &client,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/uploads/{upload_id}"),
            &token,
            Some("application/octet-stream"),
            Some(tail.len() as i64),
            tail.clone(),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{base} completing chunk: {}",
            String::from_utf8_lossy(&body)
        );
        let info: serde_json::Value = serde_json::from_slice(&body).expect("a FileInfo");
        assert_eq!(info["size"], total, "{base} completed size");
    }
}

/// A chunk whose `Content-Length` exceeds what the session still expects is the 400
/// `invalid_content_length` on both servers.
#[tokio::test]
async fn a_chunk_past_the_session_size_is_rejected() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    for base in [GO, RUST] {
        let (status, _, session) =
            create_session(&client, base, &token, &channel, "mmrs-parity-small.bin", 5).await;
        assert_eq!(status, 201, "{base} createUpload: {session}");
        let upload_id = session["id"].as_str().unwrap().to_owned();

        // Ten bytes against a five-byte session.
        let (status, served_by, body) = send(
            &client,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/uploads/{upload_id}"),
            &token,
            Some("application/octet-stream"),
            Some(10),
            vec![b'x'; 10],
        )
        .await;
        assert_eq!(
            status,
            400,
            "{base} oversize chunk: {}",
            String::from_utf8_lossy(&body)
        );
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                value["id"], "api.upload.upload_data.invalid_content_length",
                "the id"
            );
        }
    }
}

/// A declared `file_size` over `MaxFileSize` is the 413 on both servers, from `createUpload`.
#[tokio::test]
async fn an_oversize_file_size_is_refused_at_create() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    // Well past the 100 MiB default.
    let huge = 200 * 1024 * 1024;
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served_by, session) = create_session(
            &client,
            base,
            &token,
            &channel,
            "mmrs-parity-huge.bin",
            huge,
        )
        .await;
        assert_eq!(status, 413, "{base} oversize create: {session}");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
            assert_eq!(
                session["id"], "api.upload.create.upload_too_large.app_error",
                "the id"
            );
        }
        bodies.push(session);
    }
}

/// With `FileSettings.EnableFileAttachments` off, a second mm-api answers **501** to `createUpload`
/// locally — the stack Go keeps attachments on, so this is asserted against a Rust server started
/// with the override rather than compared to Go.
#[tokio::test]
async fn disabled_attachments_refuses_create_with_501() {
    if !stack_enabled() {
        return;
    }
    let Some(server) =
        common::SecondServer::start(8087, &[("MM_FILESETTINGS_ENABLEFILEATTACHMENTS", "false")])
            .await
    else {
        return;
    };
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let (status, served_by, body) = send(
        &client,
        &server.base,
        reqwest::Method::POST,
        "/api/v4/uploads",
        &token,
        Some("application/json"),
        None,
        serde_json::to_vec(&serde_json::json!({
            "channel_id": channel,
            "filename": "x.txt",
            "file_size": 3,
            "type": "attachment",
        }))
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        501,
        "disabled create: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value["id"], "api.file.attachments.disabled.app_error",
        "the id"
    );
}

/// A plain user who is a **member** of the channel — holding `upload_file` but not
/// `manage_system` — can create an upload on both servers. This is the positive case that pins the
/// permission *target*: a check against any permission the admin-and-member both hold would pass
/// here regardless, but a member who is not a system admin separates `upload_file` from
/// `manage_system`.
#[tokio::test]
async fn a_channel_member_can_create_an_upload() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "upmember").await;
    let channel = create_channel(&client, &admin, &team, "upmember").await;
    let user = create_plain_user(&client, &admin, &team, "upmember").await;
    add_user_to_channel(&client, &admin, &channel, &user.id).await;

    for base in [GO, RUST] {
        let (status, served_by, session) = create_session(
            &client,
            base,
            &user.token,
            &channel,
            "mmrs-parity-member.txt",
            4,
        )
        .await;
        assert_eq!(status, 201, "{base} member create: {session}");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
            assert_eq!(
                session["user_id"], user.id,
                "the session is owned by the member"
            );
        }
    }

    delete_plain_user(&client, &admin, &user.id).await;
}
