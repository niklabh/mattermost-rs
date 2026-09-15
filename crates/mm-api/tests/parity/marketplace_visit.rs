//! Cross-server parity for `GET` and `POST /api/v4/plugins/marketplace/first_admin_visit` —
//! `getFirstAdminVisitMarketplaceStatus` and `setFirstAdminVisitMarketplaceStatus`
//! (api4/plugin.go:434-492).
//!
//! ```sh
//! scripts/parity.sh --test parity marketplace_visit
//! ```
//!
//! The row is a single `Systems` entry shared by both servers, so the suite reads what is there,
//! removes it (the synthesised-`"false"` branch), writes it through each server in turn (the
//! stored branch, and the write itself), and restores the original at the end. No other suite
//! touches `FirstAdminVisitMarketplace`, so there is no lock to take.
//!
//! The write's other half is a broadcast; a probe on each server's websocket asserts the frame
//! arrives with the same `data` and the same all-server broadcast.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both, fetch_both_raw, go_minted_token, post_both_raw, request_raw,
    set_system_value, stack_enabled, system_value,
};

const PATH: &str = "/api/v4/plugins/marketplace/first_admin_visit";
const ROW: &str = "FirstAdminVisitMarketplace";
const EVENT: &str = "first_admin_visit_marketplace_status_received";

/// A JSON error body with its `request_id` blanked, or the raw bytes when it is not JSON.
fn scrub_request_id(body: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(mut value) => {
            if let Some(obj) = value.as_object_mut() {
                obj.remove("request_id");
            }
            value.to_string().into_bytes()
        }
        Err(_) => body.to_vec(),
    }
}

/// Put the row back the way it was found: absent, or holding its text.
async fn restore(original: Option<Option<String>>) {
    match original {
        None => set_system_value(ROW, None).await,
        Some(value) => set_system_value(ROW, value.as_deref()).await,
    };
}

/// The event frame naming the route, or none within the window.
async fn visit_event(socket: &mut SocketProbe) -> Option<serde_json::Value> {
    let found = |frames: &[serde_json::Value]| frames.iter().any(|f| f["event"] == EVENT);
    if !socket
        .collect_until(Duration::from_millis(2500), found)
        .await
    {
        return None;
    }
    socket.events_named(EVENT).into_iter().next()
}

/// An absent row reads as `"false"` and a stored one reads as stored; the write puts `"true"`
/// there through either server; the bodies are `json.NewEncoder`'s, newline and all.
#[tokio::test]
async fn the_status_round_trips_through_both_servers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(original) = system_value(ROW).await else {
        return;
    };

    // No row: both synthesise `"false"`.
    assert!(set_system_value(ROW, None).await);
    let (go, rs) = fetch_both(&client, &token, PATH).await;
    assert_eq!(
        go,
        b"{\"name\":\"FirstAdminVisitMarketplace\",\"value\":\"false\"}\n"
    );
    assert_eq!(go, rs, "the synthesised row differs");

    // A distinctive stored value, so the stored branch cannot pass as the synthesised one.
    assert!(set_system_value(ROW, Some("maybe")).await);
    let (go, rs) = fetch_both(&client, &token, PATH).await;
    assert_eq!(
        go,
        b"{\"name\":\"FirstAdminVisitMarketplace\",\"value\":\"maybe\"}\n"
    );
    assert_eq!(go, rs, "the stored row differs");

    // The write, once through each server, each time from a removed row; a probe on each side
    // sees the broadcast.
    for (base, ours) in [(GO, false), (RUST, true)] {
        assert!(set_system_value(ROW, None).await);
        let mut probe = SocketProbe::connect(base, &token).await;
        let (status, body, served_by) = request_raw(
            &client,
            base,
            reqwest::Method::POST,
            Some(&token),
            PATH,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(body, br#"{"status":"OK"}"#, "{base}");
        assert_eq!(
            served_by.as_deref() == Some("rust"),
            ours,
            "{base}: served by the wrong server"
        );
        assert_eq!(
            system_value(ROW).await,
            Some(Some(Some("true".to_owned()))),
            "{base}: the row was not written"
        );
        let frame = visit_event(&mut probe)
            .await
            .unwrap_or_else(|| panic!("{base}: no {EVENT} frame arrived"));
        assert_eq!(
            frame["data"],
            serde_json::json!({"firstAdminVisitMarketplaceStatus": "true"}),
            "{base}"
        );
        assert_eq!(
            frame["broadcast"],
            serde_json::json!({"omit_users": null, "user_id": "", "channel_id": "", "team_id": "", "connection_id": "", "omit_connection_id": ""}),
            "{base}"
        );
        let (go, rs) = fetch_both(&client, &token, PATH).await;
        assert_eq!(
            go,
            b"{\"name\":\"FirstAdminVisitMarketplace\",\"value\":\"true\"}\n"
        );
        assert_eq!(go, rs, "{base}: the written row reads back differently");
    }

    restore(original).await;
}

/// A plain user is the 403 on both routes; no session is the 401 on the `GET` and — because the
/// `POST` is `APIHandler` — the same 403 on the `POST`, with nobody in the detail.
#[tokio::test]
async fn the_refusals_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "mktv").await;
    let user = create_plain_user(&client, &admin, &team, "mktv").await;
    let plain = user.token.clone();

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &plain, PATH).await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "GET as a plain user");

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &plain, PATH, b"").await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "POST as a plain user");

    for (base, ours) in [(GO, false), (RUST, true)] {
        let (status, _, served_by) =
            request_raw(&client, base, reqwest::Method::GET, None, PATH, None).await;
        assert_eq!(status, 401, "{base}: anonymous GET");
        assert_eq!(served_by.as_deref() == Some("rust"), ours);
    }
    let anonymous = async |base: &str| {
        let (status, body, served_by) =
            request_raw(&client, base, reqwest::Method::POST, None, PATH, None).await;
        (status, body, served_by.as_deref() == Some("rust"))
    };
    let (go_status, go_body, go_ours) = anonymous(GO).await;
    let (rs_status, rs_body, rs_ours) = anonymous(RUST).await;
    assert_eq!((go_status, rs_status), (403, 403), "anonymous POST");
    assert!(!go_ours && rs_ours);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "anonymous POST");

    delete_plain_user(&client, &admin, &user.id).await;
}

/// Serving one literal under `/plugins` must not capture its neighbours: six other `/plugins`
/// requests still come back from Go.
#[tokio::test]
async fn the_other_plugin_routes_still_forward() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    for (method, path) in [
        (reqwest::Method::GET, "/api/v4/plugins"),
        (reqwest::Method::GET, "/api/v4/plugins/marketplace"),
        (reqwest::Method::GET, "/api/v4/plugins/statuses"),
        (reqwest::Method::GET, "/api/v4/plugins/webapp"),
        (reqwest::Method::POST, "/api/v4/plugins/marketplace"),
        (reqwest::Method::POST, "/api/v4/plugins/install_from_url"),
        (reqwest::Method::PUT, PATH),
        (reqwest::Method::DELETE, PATH),
    ] {
        let (go_status, go_body, _) =
            request_raw(&client, GO, method.clone(), Some(&token), path, None).await;
        let (rs_status, rs_body, served_by) =
            request_raw(&client, RUST, method.clone(), Some(&token), path, None).await;
        assert_ne!(
            served_by.as_deref(),
            Some("rust"),
            "{method} {path} was served here"
        );
        assert_eq!(go_status, rs_status, "{method} {path}");
        // Two forwarded answers are Go's twice over and differ only in `request_id`.
        assert_eq!(
            scrub_request_id(&go_body),
            scrub_request_id(&rs_body),
            "{method} {path}"
        );
    }
}
