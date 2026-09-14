//! Cross-server parity for `POST /api/v4/logs`, the client log line, with the developer flag
//! off: a session is required, the body is echoed with the message rewritten, and every
//! string-valued key survives while the others are dropped.
//!
//! ```sh
//! scripts/parity.sh --test parity client_log
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
        .post(format!("{base}/api/v4/logs"))
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

/// Both servers echo the same bytes for the same body.
async fn both_echo(client: &reqwest::Client, token: &str, body: &str) -> Vec<u8> {
    let (go_status, _, go) = post(client, GO, Some(token), body).await;
    let (rs_status, served, rs) = post(client, RUST, Some(token), body).await;
    assert_eq!(
        go_status,
        200,
        "Go {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        200,
        "Rust {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{body}: served here");
    assert_eq!(
        go,
        rs,
        "{body}: go={} rust={}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );
    go
}

/// The echo: the message prefixed, the other keys kept sorted, the non-strings dropped, the
/// long message cut at 399 bytes — for the admin and for a plain member alike — and no
/// session the 403.
#[tokio::test]
async fn the_line_is_echoed_with_the_message_rewritten() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "clog").await;
    let user = create_plain_user(&client, &admin, &team, "clog").await;

    let echoed = both_echo(
        &client,
        &user.token,
        r#"{"level":"ERROR","message":"it broke"}"#,
    )
    .await;
    assert_eq!(
        echoed,
        b"{\"level\":\"ERROR\",\"message\":\"Client Logs API Endpoint Message: it broke\"}\n"
    );
    both_echo(&client, &admin, r#"{"level":"ERROR","message":"it broke"}"#).await;
    // Sorted keys, the non-string values as `""` (the entry exists before its value fails, and
    // Go only logs the error), and a level nobody checks.
    let echoed = both_echo(
        &client,
        &user.token,
        r#"{"zeta":"z","message":"m","alpha":"a","count":3,"nested":{"x":1},"level":"error"}"#,
    )
    .await;
    assert_eq!(
        echoed,
        b"{\"alpha\":\"a\",\"count\":\"\",\"level\":\"error\",\"message\":\"Client Logs API Endpoint Message: m\",\"nested\":\"\",\"zeta\":\"z\"}\n"
    );
    // No message, no level, a non-object, and nothing: the prefix alone.
    for body in ["{}", "[]", "null", "", r#"{"message":7}"#] {
        let echoed = both_echo(&client, &user.token, body).await;
        assert_eq!(
            echoed, b"{\"message\":\"Client Logs API Endpoint Message: \"}\n",
            "{body}"
        );
    }
    // Exactly 400 in: kept whole — the cut is for messages *over* 400, at 399.
    let exact = "y".repeat(400);
    let echoed = both_echo(&client, &user.token, &format!(r#"{{"message":"{exact}"}}"#)).await;
    let parsed: serde_json::Value = serde_json::from_slice(&echoed).expect("JSON");
    assert_eq!(
        parsed["message"].as_str().map(str::len),
        Some("Client Logs API Endpoint Message: ".len() + 400)
    );
    // 500 characters in, 399 out.
    let long = "x".repeat(500);
    let echoed = both_echo(&client, &user.token, &format!(r#"{{"message":"{long}"}}"#)).await;
    let parsed: serde_json::Value = serde_json::from_slice(&echoed).expect("JSON");
    assert_eq!(
        parsed["message"].as_str().map(str::len),
        Some("Client Logs API Endpoint Message: ".len() + 399)
    );

    let (go_status, _, go) = post(&client, GO, None, r#"{"message":"m"}"#).await;
    let (rs_status, served, rs) = post(&client, RUST, None, r#"{"message":"m"}"#).await;
    assert_eq!(
        (go_status, rs_status),
        (403, 403),
        "{}",
        String::from_utf8_lossy(&go)
    );
    assert!(served);
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], "api.context.permissions.app_error");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/logs");
}
