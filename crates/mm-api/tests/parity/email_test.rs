//! Cross-server parity for `POST /api/v4/email/test` — the refusals served here and the send
//! handed to Go.
//!
//! ```sh
//! scripts/parity.sh --test parity email_test
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

/// The fixture's `EmailSettings`, every one of its thirty fields set, with `SMTPServer` replaced.
fn full_settings(smtp_server: &str) -> String {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../fixtures/config.json")).expect("JSON");
    let mut section = fixture["EmailSettings"].clone();
    section["SMTPServer"] = serde_json::Value::String(smtp_server.to_owned());
    serde_json::json!({ "EmailSettings": section }).to_string()
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: &str,
) -> (u16, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/email/test"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
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

async fn both_refuse(client: &reqwest::Client, token: &str, body: &str, status: u16, id: &str) {
    let (go_status, _, go) = post(client, GO, token, body).await;
    let (rs_status, served, rs) = post(client, RUST, token, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {id}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {id}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(served.as_deref(), Some("rust"), "{id}: served here");
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], id);
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/email/test");
}

/// A partial section is the nil-field 400 for anyone, before the permission; a full one is the
/// 403 for a plain member; a full one with no server the 400 for the admin; and a full one with
/// a server, or no decodable body at all, is Go's — which tries the SMTP send.
#[tokio::test]
async fn the_refusals_are_served_and_the_send_is_gos() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "emt").await;
    let user = create_plain_user(&client, &admin, &team, "emt").await;

    for body in [
        r#"{"EmailSettings":{"SMTPServer":"x"}}"#,
        r#"{"EmailSettings":{}}"#,
        "{}",
    ] {
        both_refuse(
            &client,
            &user.token,
            body,
            400,
            "api.file.test_connection_email_settings_nil.app_error",
        )
        .await;
        both_refuse(
            &client,
            &admin,
            body,
            400,
            "api.file.test_connection_email_settings_nil.app_error",
        )
        .await;
    }
    both_refuse(
        &client,
        &user.token,
        &full_settings("smtp.example"),
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both_refuse(
        &client,
        &admin,
        &full_settings(""),
        400,
        "api.admin.test_email.missing_server",
    )
    .await;

    // Past the refusals the send is Go's: with a server, and with no body at all.
    for body in [full_settings("127.0.0.1"), "null".to_owned(), "".to_owned()] {
        let (status, served, response) = post(&client, RUST, &admin, &body).await;
        assert_eq!(served.as_deref(), Some("go"), "{body}: forwarded");
        assert!(
            status == 200 || status == 500,
            "{body}: Go's send answered {status}: {}",
            String::from_utf8_lossy(&response)
        );
    }
}
