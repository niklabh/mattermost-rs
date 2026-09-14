//! Cross-server parity for `POST /api/v4/users/trigger-notify-admin-posts` on a server whose
//! `ServiceSettings.EnableAPITriggerAdminNotifications` is off — every request is the 403
//! `api.cloud.app_error`, whoever sends it and whatever the body, behind the session.
//!
//! ```sh
//! scripts/parity.sh --test parity notify_admin_trigger
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
        .post(format!("{base}/api/v4/users/trigger-notify-admin-posts"))
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

/// The setting refuses first: the admin, a plain user, a good body, a bad one — all the 403 —
/// and only the missing session comes before it.
#[tokio::test]
async fn the_setting_refuses_everyone_before_the_body_or_the_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "nat").await;
    let user = create_plain_user(&client, &admin, &team, "nat").await;

    for (token, body) in [
        (&admin, r#"{"trial_notification":true}"#),
        (&admin, "null"),
        (&admin, "[]"),
        (&user.token, r#"{"trial_notification":false}"#),
        (&user.token, "{}"),
    ] {
        let (go_status, _, go) = post(&client, GO, Some(token), body).await;
        let (rs_status, served, rs) = post(&client, RUST, Some(token), body).await;
        assert_eq!(
            go_status,
            403,
            "Go {body}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            403,
            "Rust {body}: {}",
            String::from_utf8_lossy(&rs)
        );
        assert!(served, "{body}: served here");
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(parsed["id"], "api.cloud.app_error", "{body}");
        assert_error_bodies_match_except_known_gaps(
            &go,
            &rs,
            "/api/v4/users/trigger-notify-admin-posts",
        );
    }

    let (go_status, _, go) = post(&client, GO, None, "{}").await;
    let (rs_status, served, rs) = post(&client, RUST, None, "{}").await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert!(served);
    assert_error_bodies_match_except_known_gaps(
        &go,
        &rs,
        "/api/v4/users/trigger-notify-admin-posts",
    );
}
