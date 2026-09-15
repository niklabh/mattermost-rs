//! Cross-server parity for the four writes of `api4/outgoing_oauth_connection.go` — `POST
//! /api/v4/oauth/outgoing_connections`, `POST …/validate`, and `PUT` / `DELETE`
//! `…/outgoing_connections/{id}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity outgoing_oauth_writes
//! ```
//!
//! Every one is the write permission then `ensureOutgoingOAuthConnectionInterface`; the
//! stack's `EnableOutgoingOAuthConnections` is off, so the admin's answer is the
//! `configuration_disabled` 501 on the unlicensed pair **and on the licensed pair** — the
//! setting is read before the licence. The `upgrade_needed` arm behind an open setting is the
//! unit test in `outgoing_oauth_writes.rs`; no shared oracle has the setting on.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, delete_plain_user,
    go_minted_token, licensed, request_raw, stack_enabled,
};

const ID: &str = "abcdefghijklmnopqrstuvwxyz";

fn routes() -> [(reqwest::Method, String); 4] {
    [
        (
            reqwest::Method::POST,
            "/api/v4/oauth/outgoing_connections".to_owned(),
        ),
        (
            reqwest::Method::POST,
            "/api/v4/oauth/outgoing_connections/validate".to_owned(),
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/oauth/outgoing_connections/{ID}"),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/oauth/outgoing_connections/{ID}"),
        ),
    ]
}

async fn both(
    client: &reqwest::Client,
    go: &str,
    rust: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, go_body, _) =
        request_raw(client, go, method.clone(), Some(token), path, Some(b"{}")).await;
    let (rs_status, rs_body, served) =
        request_raw(client, rust, method, Some(token), path, Some(b"{}")).await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path} was not served here"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// A plain user fails the single write permission on all four, before the setting is read.
#[tokio::test]
async fn a_plain_user_is_refused_the_write_permission() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "oauthwrites").await;

    for (method, path) in routes() {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, GO, RUST, method, &plain.token, &path).await;
        assert_eq!(go_status, 403, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 403, "{path}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        let go: serde_json::Value = serde_json::from_slice(&go).unwrap();
        assert_eq!(go["id"], "api.context.permissions.app_error", "{path}");
    }
    // Unlicensed Go refuses a create at 250 active users (limits.go:124); do not hold one past the test.
    delete_plain_user(&client, &admin, &plain.id).await;
}

/// The admin passes the permission and meets the closed setting: 501 `configuration_disabled`
/// on all four, whatever the body (it is never read).
#[tokio::test]
async fn the_admin_meets_the_closed_setting_unlicensed() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;

    for (method, path) in routes() {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, GO, RUST, method, &admin, &path).await;
        assert_eq!(go_status, 501, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 501, "{path}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        let go: serde_json::Value = serde_json::from_slice(&go).unwrap();
        assert_eq!(
            go["id"], "api.context.outgoing_oauth_connection.not_available.configuration_disabled",
            "{path}"
        );
    }
}

/// The same four answers on the licensed pair: the setting is read before the licence, and the
/// enterprise interface behind it is nil on every build from this tree anyway.
#[tokio::test]
async fn the_admin_meets_the_closed_setting_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;

    for (method, path) in routes() {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair.go, &pair.rust, method, &admin, &path).await;
        assert_eq!(go_status, 501, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 501, "{path}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        let go: serde_json::Value = serde_json::from_slice(&go).unwrap();
        assert_eq!(
            go["id"], "api.context.outgoing_oauth_connection.not_available.configuration_disabled",
            "{path}"
        );
    }
}
