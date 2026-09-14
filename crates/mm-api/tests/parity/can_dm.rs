//! Cross-server parity for `GET /api/v4/sharedchannels/users/{user_id}/can_dm/{other_user_id}`.
//!
//! ```sh
//! scripts/parity.sh --test parity can_dm
//! ```
//!
//! `ConnectedWorkspacesSettings.EnableSharedChannels` is off on the stack, so neither pair has a
//! shared-channel sync service and the answer past the visibility check is `true` — the
//! licensed pair included, whose licence has the feature but whose config does not. The route
//! checks no permission: any session may ask about any pair.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

async fn get(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
) -> (u16, bool, Vec<u8>) {
    let mut request = client.get(format!("{base}{path}"));
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

/// Both servers, ours serving; a success byte for byte, an error around the known gaps.
async fn both(
    client: &reqwest::Client,
    bases: (&str, &str),
    token: Option<&str>,
    path: &str,
    status: u16,
) -> Vec<u8> {
    let (go_status, _, go) = get(client, bases.0, token, path).await;
    let (rs_status, served, rs) = get(client, bases.1, token, path).await;
    assert_eq!(
        go_status,
        status,
        "Go {path}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {path}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{path}: served here");
    if status < 400 {
        assert_eq!(
            go,
            rs,
            "{path}: go={} rust={}",
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs)
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }
    go
}

fn path(user_id: &str, other_user_id: &str) -> String {
    format!("/api/v4/sharedchannels/users/{user_id}/can_dm/{other_user_id}")
}

/// With no sync service every visible pair is `true` — the other way round, the caller about
/// themselves, an id that names nobody, and another account's pair asked by a third party.
#[tokio::test]
async fn without_a_sync_service_every_visible_pair_can_dm() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cdm").await;
    let a = create_plain_user(&client, &admin, &team, "cdma").await;
    let b = create_plain_user(&client, &admin, &team, "cdmb").await;
    let nobody = mm_model::utils::new_id();

    for (token, user, other) in [
        (&a.token, &a.id, &b.id),
        (&b.token, &b.id, &a.id),
        (&a.token, &a.id, &a.id),
        (&a.token, &a.id, &nobody),
        (&a.token, &b.id, &a.id),
        (&admin, &a.id, &b.id),
    ] {
        let body = both(&client, (GO, RUST), Some(token), &path(user, other), 200).await;
        assert_eq!(
            body, b"{\"can_dm\":true}\n",
            "json.NewEncoder, with the newline"
        );
    }

    // The licensed pair: the licence has shared channels, the config does not, so still no
    // service and still `true`.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let body = both(
        &client,
        (&pair.go, &pair.rust),
        Some(&a.token),
        &path(&a.id, &b.id),
        200,
    )
    .await;
    assert_eq!(body, b"{\"can_dm\":true}\n");
}

/// The two id checks in order, and the session requirement.
#[tokio::test]
async fn the_ids_are_checked_in_order_behind_the_session() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cdmi").await;
    let a = create_plain_user(&client, &admin, &team, "cdmi").await;

    let body = both(
        &client,
        (GO, RUST),
        Some(&a.token),
        &path("abc", &a.id),
        400,
    )
    .await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    assert!(
        parsed["message"]
            .as_str()
            .is_some_and(|m| m.contains("user_id") && !m.contains("other_user_id")),
        "the first id first: {parsed}"
    );
    let body = both(
        &client,
        (GO, RUST),
        Some(&a.token),
        &path(&a.id, "abc"),
        400,
    )
    .await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert!(
        parsed["message"]
            .as_str()
            .is_some_and(|m| m.contains("other_user_id")),
        "the second id: {parsed}"
    );
    // Both bad: the first wins.
    let body = both(
        &client,
        (GO, RUST),
        Some(&a.token),
        &path("abc", "def"),
        400,
    )
    .await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert!(
        parsed["message"]
            .as_str()
            .is_some_and(|m| !m.contains("other_user_id")),
        "{parsed}"
    );

    let body = both(&client, (GO, RUST), None, &path(&a.id, &a.id), 401).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(parsed["id"], "api.context.session_expired.app_error");
}
