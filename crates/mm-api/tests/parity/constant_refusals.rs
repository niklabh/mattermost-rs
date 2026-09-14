//! Cross-server parity for two routes that are a constant refusal on this deployment:
//! `POST /api/v4/cloud/webhook` (no Cloud licence, the 401 `TokenRequired`) and
//! `POST /api/v4/users/login/sso/code-exchange` (the feature flag off, the 410 with a
//! `Deprecation` header).
//!
//! ```sh
//! scripts/parity.sh --test parity constant_refusals
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, bool, reqwest::header::HeaderMap, Vec<u8>) {
    let mut request = client
        .post(format!("{base}{path}"))
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
    let headers = response.headers().clone();
    let served = headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        headers,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// The webhook: the 401 for nobody and for the admin alike, on both pairs — the Enterprise
/// licence is not a Cloud one.
#[tokio::test]
async fn the_cws_webhook_is_the_401_without_a_cloud_licence() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let path = "/api/v4/cloud/webhook";

    for token in [None, Some(admin.as_str())] {
        let (go_status, _, _, go) = post(&client, GO, path, token, r#"{"event":"x"}"#).await;
        let (rs_status, served, _, rs) = post(&client, RUST, path, token, r#"{"event":"x"}"#).await;
        assert_eq!(go_status, 401, "Go: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 401, "Rust: {}", String::from_utf8_lossy(&rs));
        assert!(served, "served here");
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(parsed["id"], "api.context.session_expired.app_error");
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let (go_status, _, _, go) = post(&client, &pair.go, path, Some(&admin), "{}").await;
    let (rs_status, served, _, rs) = post(&client, &pair.rust, path, Some(&admin), "{}").await;
    assert_eq!(
        (go_status, rs_status),
        (401, 401),
        "licensed: {}",
        String::from_utf8_lossy(&go)
    );
    assert!(served);
    assert_error_bodies_match_except_known_gaps(&go, &rs, path);
}

/// The code exchange: the 410 with `Deprecation: true`, session or not, body or not.
#[tokio::test]
async fn the_sso_code_exchange_is_the_410_with_the_deprecation_header() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let path = "/api/v4/users/login/sso/code-exchange";

    for (token, body) in [
        (
            None,
            r#"{"login_code":"a","code_verifier":"b","state":"c"}"#,
        ),
        (Some(admin.as_str()), "{}"),
        (None, ""),
    ] {
        let (go_status, _, go_headers, go) = post(&client, GO, path, token, body).await;
        let (rs_status, served, rs_headers, rs) = post(&client, RUST, path, token, body).await;
        assert_eq!(
            go_status,
            410,
            "Go {body}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            410,
            "Rust {body}: {}",
            String::from_utf8_lossy(&rs)
        );
        assert!(served, "{body}: served here");
        assert_eq!(
            go_headers.get("Deprecation").map(|v| v.as_bytes()),
            Some(&b"true"[..]),
            "Go sets Deprecation"
        );
        assert_eq!(
            rs_headers.get("Deprecation").map(|v| v.as_bytes()),
            Some(&b"true"[..]),
            "so do we"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(
            parsed["id"],
            "api.user.login_sso_code_exchange.deprecated.app_error"
        );
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }
}
