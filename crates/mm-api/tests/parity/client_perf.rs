//! Cross-server parity for `POST /api/v4/client_perf` — on every server this project runs the
//! metrics sink is absent, so a session is the only thing the route looks at.
//!
//! ```sh
//! scripts/parity.sh --test parity client_perf
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    body: &'static str,
) -> (u16, bool, Option<String>, Vec<u8>) {
    let mut request = client
        .post(format!("{base}/api/v4/client_perf"))
        .header("Content-Type", "application/json")
        .body(body);
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
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        served,
        content_type,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// A valid report, an invalid one and a body that is not JSON: 200 and nothing, every time.
#[tokio::test]
async fn every_body_is_accepted_with_an_empty_answer() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;

    for body in [
        r#"{"version":"0.1.0","client_id":"x","labels":{"platform":"web"},"start":1,"end":2,"counters":[],"histograms":[]}"#,
        r#"{"version":"not-semver","start":9,"end":1}"#,
        "not json at all",
    ] {
        let (go_status, _, go_type, go_body) = post(&client, GO, Some(&admin), body).await;
        let (rs_status, served, rs_type, rs_body) = post(&client, RUST, Some(&admin), body).await;
        assert!(served, "{body}: was forwarded to Go");
        assert_eq!((go_status, rs_status), (200, 200), "{body}");
        assert!(
            go_body.is_empty(),
            "{body}: Go answered {:?}",
            String::from_utf8_lossy(&go_body)
        );
        assert!(
            rs_body.is_empty(),
            "{body}: we answered {:?}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(go_type, rs_type, "{body}: content type");
    }
}

/// No session is the 401 on both.
#[tokio::test]
async fn no_session_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let (go_status, _, _, go_body) = post(&client, GO, None, "{}").await;
    let (rs_status, _, _, rs_body) = post(&client, RUST, None, "{}").await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no session");
}
