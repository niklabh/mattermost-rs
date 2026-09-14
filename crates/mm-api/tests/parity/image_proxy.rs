//! Cross-server parity for `GET /api/v4/image` on a deployment with the image proxy off: every
//! `url` is the same 400, and no session is the 401.
//!
//! ```sh
//! scripts/parity.sh --test parity image_proxy
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, stack_enabled,
};

async fn get(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    url: Option<&str>,
) -> (u16, bool, Vec<u8>) {
    let mut request = client.get(format!("{base}/api/v4/image"));
    if let Some(url) = url {
        request = request.query(&[("url", url)]);
    }
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

/// A parseable remote URL, a same-host path, an opaque URL, a URL that does not parse, and no
/// `url` at all: the one 400, byte for byte apart from the request id.
#[tokio::test]
async fn every_url_is_the_same_400_with_the_proxy_off() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;

    for url in [
        Some("http://example.com/a.png"),
        Some("/static/images/a.png"),
        Some("mailto:someone@example.com"),
        Some("%zz"),
        Some(""),
        None,
    ] {
        let (go_status, _, go_body) = get(&client, GO, Some(&admin), url).await;
        let (rs_status, served, rs_body) = get(&client, RUST, Some(&admin), url).await;
        assert!(served, "{url:?}: was forwarded to Go");
        assert_eq!((go_status, rs_status), (400, 400), "{url:?}");
        let body = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "image");
        assert_eq!(body["id"], "api.image.get.app_error", "{url:?}");
    }
}

/// No session is the 401 on both.
#[tokio::test]
async fn no_session_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let (go_status, _, go_body) = get(&client, GO, None, Some("http://example.com/a.png")).await;
    let (rs_status, _, rs_body) = get(&client, RUST, None, Some("http://example.com/a.png")).await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no session");
}
