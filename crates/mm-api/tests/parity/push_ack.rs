//! Cross-server parity for `POST /api/v4/notifications/ack` on a server whose
//! `EmailSettings.SendPushNotifications` is off — every well-formed ack is the 501, every
//! malformed one the parse 400, and the route needs a session.
//!
//! ```sh
//! scripts/parity.sh --test parity push_ack
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let mut request = client
        .post(format!("{base}/api/v4/notifications/ack"))
        .header("Content-Type", "application/json")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

async fn both(client: &reqwest::Client, token: Option<&str>, body: &str, status: u16, id: &str) {
    let (go_status, _, go) = post(client, GO, token, body).await;
    let (rs_status, served, rs) = post(client, RUST, token, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{body}: served here");
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], id, "{body}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/notifications/ack");
}

/// The well-formed acks — a full one, an empty object, and `null`, which decodes to nothing —
/// are the 501; the malformed ones the parse 400; no session the 401.
#[tokio::test]
async fn every_ack_is_the_disabled_501_and_the_malformed_ones_the_parse_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "pack").await;
    let user = create_plain_user(&client, &admin, &team, "pack").await;

    for body in [
        r#"{"id":"abc","received_at":1,"platform":"ios","type":"message","post_id":"","is_id_loaded":false}"#,
        "{}",
        "null",
        r#"{"unknown":1}"#,
    ] {
        both(
            &client,
            Some(&user.token),
            body,
            501,
            "api.push_notification.disabled.app_error",
        )
        .await;
    }
    for body in [
        "[]",
        "\"x\"",
        "",
        r#"{"received_at":"soon"}"#,
        r#"{"is_id_loaded":"yes"}"#,
    ] {
        both(
            &client,
            Some(&user.token),
            body,
            400,
            "api.push_notifications_ack.message.parse.app_error",
        )
        .await;
    }
    both(
        &client,
        None,
        "{}",
        401,
        "api.context.session_expired.app_error",
    )
    .await;
}
