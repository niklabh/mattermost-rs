//! Cross-server parity for `POST /api/v4/site_url/test`.
//!
//! ```sh
//! scripts/parity.sh --test parity site_url_test
//! ```
//!
//! Both servers ping the URL they are handed, so the stack's own Go base is the reachable case
//! and a closed port the unreachable one. No fixture is written.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/site_url/test"))
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

async fn both(client: &reqwest::Client, token: &str, body: &str, status: u16, id: Option<&str>) {
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
    match id {
        Some(id) => {
            let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
            assert_eq!(parsed["id"], id, "{body}");
            assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/site_url/test");
        }
        None => {
            assert_eq!(go, rs, "{body}: byte for byte");
            assert_eq!(go, br#"{"status":"OK"}"#, "ReturnStatusOK, no newline");
        }
    }
}

/// The reachable URL is OK, the unreachable and the non-200 ones the failure 400, the missing
/// one the parameter 400, and a plain member the 403.
#[tokio::test]
async fn a_reachable_ping_is_ok_and_everything_else_is_its_own_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "surl").await;
    let user = create_plain_user(&client, &admin, &team, "surl").await;

    both(
        &client,
        &admin,
        &format!(r#"{{"site_url":"{GO}"}}"#),
        200,
        None,
    )
    .await;
    // The proxy's own base pings too — the route under test forwards nothing on the way.
    both(
        &client,
        &admin,
        &format!(r#"{{"site_url":"{RUST}"}}"#),
        200,
        None,
    )
    .await;
    for url in [
        "http://127.0.0.1:1".to_owned(),
        format!("{GO}/nowhere"),
        "not a url".to_owned(),
    ] {
        both(
            &client,
            &admin,
            &format!(r#"{{"site_url":"{url}"}}"#),
            400,
            Some("app.admin.test_site_url.failure"),
        )
        .await;
    }
    for body in ["{}", r#"{"site_url":""}"#, "[]", "", r#"{"site_url":5}"#] {
        both(
            &client,
            &admin,
            body,
            400,
            Some("api.context.invalid_body_param.app_error"),
        )
        .await;
    }
    both(
        &client,
        &user.token,
        &format!(r#"{{"site_url":"{GO}"}}"#),
        403,
        Some("api.context.permissions.app_error"),
    )
    .await;
}
