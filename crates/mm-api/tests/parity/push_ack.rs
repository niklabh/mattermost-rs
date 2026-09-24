//! Cross-server parity for `POST /api/v4/notifications/ack`. The stack runs with push **on**
//! (`scripts/stack-env.sh`), so a well-formed ack goes to the push proxy and answers OK; the
//! disabled 501 is `mm_api::push_ack`'s unit territory. A malformed ack is the parse 400, and the
//! route needs a session.
//!
//! ```sh
//! scripts/parity.sh --test parity push_ack
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
        .post(format!("{base}/api/v4/notifications/ack"))
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

async fn both(client: &reqwest::Client, token: Option<&str>, body: &str, status: u16, id: &str) {
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
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], id, "{body}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/notifications/ack");
}

/// Both servers' `(status, body)` for one ack, compared whole.
async fn both_ok(client: &reqwest::Client, token: &str, body: &str) -> (u16, Vec<u8>) {
    let (go_status, _, go) = post(client, GO, Some(token), body).await;
    let (rs_status, served, rs) = post(client, RUST, Some(token), body).await;
    assert!(served, "{body}: served here");
    // An error body carries the request's own id; nothing else may differ.
    let without_request_id = |bytes: &[u8]| match serde_json::from_slice::<serde_json::Value>(bytes)
    {
        Ok(serde_json::Value::Object(mut map)) => {
            map.remove("request_id");
            serde_json::Value::Object(map).to_string()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    assert_eq!(
        (go_status, without_request_id(&go)),
        (rs_status, without_request_id(&rs)),
        "{body}"
    );
    (go_status, go)
}

/// The well-formed acks — a full one, an empty object, and `null`, which decodes to nothing —
/// reach the proxy and answer OK; the malformed ones the parse 400; no session the 401.
#[tokio::test]
async fn a_well_formed_ack_is_ok_and_the_malformed_ones_the_parse_400() {
    if !stack_enabled() {
        return;
    }
    let _proxy = common::push_proxy::push_proxy().expect("the push proxy");
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "pack").await;
    let user = create_plain_user(&client, &admin, &team, "pack").await;

    for body in [
        r#"{"id":"abc","received_at":1,"platform":"ios","type":"message","post_id":"","is_id_loaded":false}"#,
        "{}",
        "null",
        r#"{"unknown":1}"#,
    ] {
        let (status, answer) = both_ok(&client, &user.token, body).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(answer, br#"{"status":"OK"}"#, "{body}");
    }
    for body in [
        "[]",
        "\"x\"",
        "",
        r#"{"received_at":"soon"}"#,
        r#"{"is_id_loaded":"yes"}"#,
    ] {
        both(
            &client,
            Some(&user.token),
            body,
            400,
            "api.push_notifications_ack.message.parse.app_error",
        )
        .await;
    }
    both(
        &client,
        None,
        "{}",
        401,
        "api.context.session_expired.app_error",
    )
    .await;
}

/// An **id-loaded** ack never fails on the proxy and writes **nothing** — an empty 200 — unless
/// it is a `message` ack naming a post; then the post is read with the caller's rights (a 404 for
/// one that does not exist) and, readable, the Enterprise id-loaded interface Go does not have
/// answers the 302 `api.system.id_loaded.not_available.app_error`.
#[tokio::test]
async fn an_id_loaded_ack_answers_empty_or_with_the_missing_interface() {
    if !stack_enabled() {
        return;
    }
    let _proxy = common::push_proxy::push_proxy().expect("the push proxy");
    let client = client();
    let admin = go_minted_token(&client).await;
    let (_team, channel) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let post_id = common::post_message(&client, &admin, &channel, "id loaded", None).await;

    let (status, body) = both_ok(&client, &admin, r#"{"type":"clear","is_id_loaded":true}"#).await;
    assert_eq!((status, body.len()), (200, 0), "nothing is written");

    let (status, _) = both_ok(
        &client,
        &admin,
        r#"{"type":"message","is_id_loaded":true,"post_id":"mmrsnopostmmrsnopostmmrsno"}"#,
    )
    .await;
    assert_eq!(status, 404);

    let (status, body) = both_ok(
        &client,
        &admin,
        &format!(r#"{{"type":"message","is_id_loaded":true,"post_id":"{post_id}"}}"#),
    )
    .await;
    assert_eq!(status, 302, "{}", String::from_utf8_lossy(&body));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(parsed["id"], "api.system.id_loaded.not_available.app_error");
}
