//! Cross-server parity for `GET /api/v4/redirect_location`, on a deployment whose allow-list
//! is empty — every address a test can reach is inside the guard's reserved ranges, so every
//! probe is a refusal and an empty location, cached; the parameter 400 and the session are the
//! rest.
//!
//! ```sh
//! scripts/parity.sh --test parity redirect_location
//! ```

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

async fn get(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    url: Option<&str>,
) -> (u16, bool, Vec<u8>) {
    let mut request = client.get(format!("{base}/api/v4/redirect_location"));
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

/// Loopback with a real redirect behind it, a closed port, a name that does not resolve — all
/// the empty location, byte for byte and twice (the second from the cache); no url the 400; no
/// session the 401.
#[tokio::test]
async fn internal_addresses_are_refused_into_an_empty_location() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "rloc").await;
    let user = create_plain_user(&client, &admin, &team, "rloc").await;

    let urls = [
        // Go answers this with a 301 and a `Location`, which the guard never lets either
        // server see: 127.0.0.1 is reserved and `localhost` is not in the allow-list.
        format!("{GO}//api/v4/system/ping"),
        "http://127.0.0.1:1/".to_owned(),
        "http://nowhere.mmrs.invalid/".to_owned(),
        "not a url".to_owned(),
    ];
    for url in &urls {
        for _ in 0..2 {
            let (go_status, _, go) = get(&client, GO, Some(&user.token), Some(url)).await;
            let (rs_status, served, rs) = get(&client, RUST, Some(&user.token), Some(url)).await;
            assert_eq!(go_status, 200, "Go {url}: {}", String::from_utf8_lossy(&go));
            assert_eq!(
                rs_status,
                200,
                "Rust {url}: {}",
                String::from_utf8_lossy(&rs)
            );
            assert!(served, "{url}: served here");
            assert_eq!(go, br#"{"location":""}"#, "{url}: MapToJSON, no newline");
            assert_eq!(go, rs, "{url}");
        }
    }

    for query in [None, Some("")] {
        let (go_status, _, go) = get(&client, GO, Some(&user.token), query).await;
        let (rs_status, served, rs) = get(&client, RUST, Some(&user.token), query).await;
        assert_eq!(
            (go_status, rs_status),
            (400, 400),
            "{query:?}: {}",
            String::from_utf8_lossy(&go)
        );
        assert!(served);
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
        assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/redirect_location");
    }

    let (go_status, _, go) = get(&client, GO, None, Some("http://127.0.0.1:1/")).await;
    let (rs_status, served, rs) = get(&client, RUST, None, Some("http://127.0.0.1:1/")).await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert!(served);
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/redirect_location");
}
