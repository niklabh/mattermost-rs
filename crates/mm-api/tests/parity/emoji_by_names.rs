//! Cross-server parity for `POST /api/v4/emoji/names` — `getEmojisByNames`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity emoji_by_names
//! ```
//!
//! # It answers about custom emoji only
//!
//! Every **system** emoji name is filtered out of the request before the query runs, so asking
//! for `+1` is not an error and not a hit — it is an empty answer.
//! [`system_names_are_filtered_out_of_the_request`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    go_minted_token, logged_in_user_id, post_both_raw, purge_api_fixtures, stack_enabled,
    unique_emoji_name,
};

const PATH: &str = "/api/v4/emoji/names";

struct Fixture {
    alive: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            // The name is the fixture's handle, not the id this returns — the route looks up
            // by name.
            let alive = unique_emoji_name("bynames");
            create_custom_emoji(client, token, logged_in_user_id(), &alive).await;
            Fixture { alive }
        })
        .await
}

fn body(names: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&names).expect("a JSON array")
}

/// A custom emoji comes back byte for byte, with the encoder's newline.
#[tokio::test]
async fn a_custom_emoji_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, PATH, &body(&[&f.alive])).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "`json.NewEncoder(w).Encode` appends the newline the emoji GETs do not"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed.as_array().expect("an array").len(), 1, "{parsed}");
    assert_eq!(parsed[0]["name"], f.alive.as_str());
}

/// A built-in name is dropped before the query, so it is neither an error nor a hit.
#[tokio::test]
async fn system_names_are_filtered_out_of_the_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Only built-ins: the filtered list is empty and the app layer returns before querying.
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, PATH, &body(&["+1", "smile"])).await;
    assert_eq!(go_status, 200, "asking for a built-in is not an error");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        go, b"[]\n",
        "an empty answer is `[]`, not `null` — both branches allocate"
    );

    // Mixed: the built-in is dropped and the custom one survives.
    let ((_, mixed_go), (_, mixed_rs)) =
        post_both_raw(&client, &token, PATH, &body(&["+1", &f.alive])).await;
    assert_eq!(
        String::from_utf8_lossy(&mixed_go),
        String::from_utf8_lossy(&mixed_rs)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&mixed_go).expect("JSON");
    assert_eq!(parsed.as_array().expect("an array").len(), 1, "{parsed}");
    assert_eq!(parsed[0]["name"], f.alive.as_str());
}

/// A name that matches no emoji at all is simply absent.
#[tokio::test]
async fn an_unknown_name_is_dropped_rather_than_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(&[&f.alive, "mmrsparitynosuchemoji"]),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed.as_array().expect("an array").len(), 1, "{parsed}");
}

/// The decode 400 and the empty-list 400, in Go's order.
#[tokio::test]
async fn a_bad_body_and_an_empty_list_are_the_two_400s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for (raw, id) in [
        (&b"not json"[..], "api.payload.parse.error"),
        (&b"[1,2]"[..], "api.payload.parse.error"),
        (&b"[]"[..], "api.context.invalid_body_param.app_error"),
        (&b"null"[..], "api.context.invalid_body_param.app_error"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "{shown} must be rejected by Go");
        assert_eq!(rs_status, go_status, "{shown}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], id, "for the body {shown}");
    }
}

/// Two hundred names is the cap, and 201 is a 400 — after the config gate, not before it.
#[tokio::test]
async fn two_hundred_and_one_names_is_the_too_many_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let names: Vec<String> = (0..201).map(|n| format!("mmrsparityname{n:03}")).collect();
    let asked: Vec<&str> = names.iter().map(String::as_str).collect();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&asked)).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(
        go["id"],
        "api.emoji.get_multiple_by_name_too_many.request_error"
    );

    // And exactly 200 is not.
    let asked: Vec<&str> = names.iter().take(200).map(String::as_str).collect();
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, PATH, &body(&asked)).await;
    assert_eq!(go_status, 200, "the boundary is `> 200`, not `>= 200`");
    assert_eq!(rs_status, go_status);
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
}

/// No session is a 401 on both, before the body is read.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{PATH}"))
            .json(&serde_json::json!(["smile"]))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{PATH}");
    }
}

/// `GET` on this path is **served**, as Go's method fallthrough; the rest are forwarded.
///
/// Go registers `/emoji/names` for `POST` only, so a `GET` fails the method match and gorilla
/// continues to `/emoji/{emoji_id}`, where `RequireEmojiId` rejects the literal. axum has no
/// such fallthrough — a registered literal answers every method — so the 400 is spelled out in
/// the handler, and `emoji_get`'s own suite asserts the two servers agree on it.
#[tokio::test]
async fn a_get_falls_through_to_the_id_route_and_the_rest_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let rs = client
        .get(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "GET {PATH} is `getEmoji` with a bad id, not a forward"
    );
    assert_eq!(rs.status().as_u16(), 400);

    for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {PATH} must be forwarded"
        );
    }
}
