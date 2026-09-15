//! Cross-server parity for `GET /api/v4/latest_version` — both servers ask GitHub for the same
//! release and must render it identically; the admin gate and the session are the rest.
//!
//! ```sh
//! scripts/parity.sh --test parity latest_version
//! ```
//!
//! **Needs the network.** GitHub is the oracle's oracle: without it both servers 500 in their
//! own words, which is not a comparison this suite makes.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, go_minted_token, stack_enabled,
};

async fn get(client: &reqwest::Client, base: &str, token: Option<&str>) -> (u16, bool, Vec<u8>) {
    let mut request = client.get(format!("{base}/api/v4/latest_version"));
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

/// The admin's release, byte for byte, twice — the second answer is each server's own cache.
#[tokio::test]
async fn the_release_matches_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;

    let (go_status, _, go_body) = get(&client, GO, Some(&admin)).await;
    let (rs_status, served, rs_body) = get(&client, RUST, Some(&admin)).await;
    assert!(served, "GET /latest_version was forwarded to Go");
    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        200,
        "ours: {}",
        String::from_utf8_lossy(&rs_body)
    );
    if go_body != rs_body {
        // **Go keeps the release for 24 hours and nothing clears it** — `latestVersionCache` has
        // one writer and `clearLatestVersionCache` has no caller at the pinned SHA, so neither
        // `POST /caches/invalidate` nor anything else refreshes it short of a restart. Every run
        // starts a fresh mm-api, so when GitHub moves its "latest" flag inside Go's day the two
        // servers disagree while both are correct for when they asked. Measured 2026-09-15: Go
        // held v11.10.1 from its boot, GitHub had since marked v11.9.2 latest, and we served that.
        //
        // So a difference is accepted only when GitHub, asked now, agrees with **us**. A port that
        // fetched the wrong thing still fails, since GitHub will not name its release.
        let ours: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
        let github: serde_json::Value = client
            .get(mm_api::latest_version::LATEST_VERSION_URL)
            .header("User-Agent", "mmrs-parity")
            .send()
            .await
            .expect("GitHub answers")
            .json()
            .await
            .expect("GitHub's release decodes");
        assert_eq!(
            (github["id"].as_i64(), github["tag_name"].as_str()),
            (ours["id"].as_i64(), ours["tag_name"].as_str()),
            "the release differs, and GitHub's current one is not ours either — Go: {} ours: {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
    }

    // It is a real release — otherwise both servers agreeing proves little.
    let release: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_ne!(release["id"].as_i64().unwrap_or(0), 0, "{release}");
    assert!(
        release["html_url"]
            .as_str()
            .unwrap_or("")
            .starts_with("https://github.com/")
    );
    assert!(!rs_body.ends_with(b"\n"), "json.Marshal writes no newline");

    let (_, _, rs_again) = get(&client, RUST, Some(&admin)).await;
    assert_eq!(
        rs_again, rs_body,
        "the cached answer differs from the first"
    );
}

/// A member is refused with the permission error; no session is the 401.
#[tokio::test]
async fn a_member_and_no_session_are_refused_alike() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "lver").await;
    let user = create_plain_user(&client, &admin, &team, "lver").await;

    let (go_status, _, go_body) = get(&client, GO, Some(&user.token)).await;
    let (rs_status, served, rs_body) = get(&client, RUST, Some(&user.token)).await;
    assert!(served, "the member's request was forwarded to Go");
    assert_eq!((go_status, rs_status), (403, 403));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "member");

    let (go_status, _, go_body) = get(&client, GO, None).await;
    let (rs_status, _, rs_body) = get(&client, RUST, None).await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no session");

    delete_plain_user(&client, &admin, &user.id).await;
}
