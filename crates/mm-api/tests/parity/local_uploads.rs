//! Cross-server parity for the two `POST`s of `api4/upload_local.go` on the unix socket:
//! `POST /api/v4/uploads` (`createUpload`) and `POST /api/v4/uploads/{upload_id}` (`uploadData`),
//! both through `APILocal`.
//!
//! ```sh
//! scripts/parity.sh --test parity local_uploads
//! ```
//!
//! What the empty local user does to each is the whole suite: a create is a 400 on the session's
//! `user_id` whatever the type; a data upload into an **attachment** session is the 403 the
//! handler gives a non-owner; a data upload into an **import** session goes through, because
//! that branch asks for `manage_system` — which the socket holds — and nothing about the user.
//! The import sessions are created over HTTP by the admin (one per server, each fed over its
//! own socket) and the two `FileInfo`s are compared with their per-upload fields removed.

use crate::common;

use common::local_socket::{go_socket, rust_socket, sockets_enabled};
use common::{GO, a_team_and_channel_the_user_is_in, client, go_minted_token, stack_enabled};

/// One request over one socket: `(status, served-by header, body)`.
async fn send(
    socket: &std::path::Path,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Option<String>, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", content_type)
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body))
        .expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("POST {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, served_by, body)
}

/// The same request over both sockets; asserts ours served it. Returns `(go, rust)`.
async fn both(path: &str, content_type: &str, body: &[u8]) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let (go_status, _, go_body) = send(&go, path, content_type, body.to_vec()).await;
    let (rs_status, served_by, rs_body) = send(&rust, path, content_type, body.to_vec()).await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "POST {path} was forwarded over the socket"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// An error body with its `request_id` blanked, for comparing two servers' refusals.
fn scrubbed(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).unwrap_or_else(|e| panic!("not JSON: {e}"));
    if let Some(obj) = value.as_object_mut() {
        obj.remove("request_id");
        // Our `message` is the raw id until i18n lands (D-092).
        obj.remove("message");
    }
    value
}

/// A session created over HTTP by the admin, on Go, so both sockets see the same row.
async fn create_over_http(
    client: &reqwest::Client,
    token: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{GO}/api/v4/uploads"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 201, "the upload session was not created");
    response.json().await.expect("a session")
}

/// Drop the fields two independent uploads cannot share.
fn normalise(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for key in ["id", "create_at", "update_at"] {
            obj.remove(key);
        }
    }
    value
}

/// Neither an attachment nor an import can be created over the socket: the session's user is
/// empty and `UploadSession.IsValid` refuses it on both servers with the same 400.
#[tokio::test]
async fn a_local_create_is_refused_on_the_empty_user() {
    if !stack_enabled() || !sockets_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    for body in [
        serde_json::json!({"channel_id": channel, "filename": "mmrs-local.txt", "file_size": 5, "type": "attachment"}),
        serde_json::json!({"filename": "mmrs-local.zip", "file_size": 5, "type": "import"}),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both(
            "/api/v4/uploads",
            "application/json",
            body.to_string().as_bytes(),
        )
        .await;
        assert_eq!((go_status, rs_status), (400, 400), "{body}");
        let go = scrubbed(&go_body);
        assert_eq!(
            go["id"], "model.upload_session.is_valid.user_id.app_error",
            "{body}: Go's refusal is not the user_id one"
        );
        assert_eq!(go, scrubbed(&rs_body), "{body}");
    }

    // A body that does not decode is refused before any of that, on both.
    let ((go_status, go_body), (rs_status, rs_body)) =
        both("/api/v4/uploads", "application/json", b"{not json").await;
    assert_eq!((go_status, rs_status), (400, 400));
    assert_eq!(scrubbed(&go_body), scrubbed(&rs_body));
}

/// An attachment session is somebody's, and the socket is nobody: the 403 `upload_file`. An
/// import session is the `manage_system` branch, which the socket passes, and the bytes land.
#[tokio::test]
async fn a_local_upload_feeds_an_import_and_refuses_an_attachment() {
    if !stack_enabled() || !sockets_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;
    let payload = b"mmrs local upload bytes\n".to_vec();

    let attachment = create_over_http(
        &client,
        &token,
        serde_json::json!({
            "channel_id": channel,
            "filename": "mmrs-local-attachment.txt",
            "file_size": payload.len(),
            "type": "attachment",
        }),
    )
    .await;
    let id = attachment["id"].as_str().expect("an id");
    let ((go_status, go_body), (rs_status, rs_body)) = both(
        &format!("/api/v4/uploads/{id}"),
        "application/octet-stream",
        &payload,
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = scrubbed(&go_body);
    assert_eq!(go["id"], "api.context.permissions.app_error");
    assert_eq!(go, scrubbed(&rs_body));

    // One import session per server, each fed over its own socket.
    let go_sock = go_socket().expect("checked");
    let rs_sock = rust_socket().expect("checked");
    let mut infos = Vec::new();
    for (socket, ours) in [(&go_sock, false), (&rs_sock, true)] {
        let session = create_over_http(
            &client,
            &token,
            serde_json::json!({
                "filename": "mmrs-local-import.zip",
                "file_size": payload.len(),
                "type": "import",
            }),
        )
        .await;
        let id = session["id"].as_str().expect("an id").to_owned();
        let (status, served_by, body) = send(
            socket,
            &format!("/api/v4/uploads/{id}"),
            "application/octet-stream",
            payload.clone(),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{}: {}",
            socket.display(),
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            served_by.as_deref() == Some("rust"),
            ours,
            "{}",
            socket.display()
        );
        assert_eq!(body.last(), Some(&b'\n'), "json.NewEncoder's newline");
        let info: serde_json::Value = serde_json::from_slice(&body).expect("a FileInfo");
        assert_eq!(info["name"], "mmrs-local-import.zip");
        assert_eq!(info["size"], payload.len());
        infos.push(normalise(info));

        // The completed upload is `<ImportSettings.Directory>/<session id>_<filename>`, in the
        // file store both servers share. `GET /imports` lists that directory in filesystem order
        // on Go and sorted here, so a second leftover file makes the two listings differ for
        // every suite that reads them. Remove it through Go's own delete.
        let name = format!("{id}_mmrs-local-import.zip");
        let deleted = client
            .delete(format!("{GO}/api/v4/imports/{name}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(deleted.status(), 200, "the import file {name} is removed");
    }
    assert_eq!(infos[0], infos[1], "the two import FileInfos differ");
}
