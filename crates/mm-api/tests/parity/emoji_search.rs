//! Cross-server parity for `POST /api/v4/emoji/search` — `searchEmojis`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity emoji_search
//! ```
//!
//! # The two 400s are one answer
//!
//! A body that does not decode and an empty term are `SetInvalidParamWithErr("term", …)` and
//! `SetInvalidParam("term")` — the same id and the same parameter — so a client cannot tell them
//! apart. [`every_bad_body_is_the_same_400`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    fetch_both_raw, go_minted_token, logged_in_user_id, post_both_raw, purge_api_fixtures,
    stack_enabled, unique_emoji_name,
};

const PATH: &str = "/api/v4/emoji/search";

struct Fixture {
    /// `mmrsparitysearch<stamp>middle`, so a prefix search and a substring search differ.
    middle: String,
    /// Shares the fixture's stamp prefix, so a prefix search finds both.
    prefixed: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let stem = unique_emoji_name("search");
            let prefixed = format!("{stem}aaa");
            let middle = format!("{stem}zzzmiddle");
            for name in [&prefixed, &middle] {
                create_custom_emoji(client, token, logged_in_user_id(), name).await;
            }
            Fixture {
                middle,
                prefixed: prefixed.clone(),
            }
        })
        .await
}

fn body(term: &str, prefix_only: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"term": term, "prefix_only": prefix_only}))
        .expect("JSON")
}

fn names(raw: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .expect("JSON")
        .as_array()
        .expect("an array")
        .iter()
        .map(|e| e["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A substring search finds both fixture emoji, byte for byte.
#[tokio::test]
async fn a_substring_search_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, PATH, &body("zzzmiddle", false)).await;
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
        "`json.NewEncoder(w).Encode` appends the newline"
    );
    assert_eq!(names(&go), vec![f.middle.clone()], "{:?}", names(&go));
}

/// `prefix_only` moves the wildcard, and the two answers differ.
#[tokio::test]
async fn prefix_only_anchors_the_pattern() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The middle emoji's tail matches as a substring and not as a prefix.
    let ((_, substring), (_, substring_rs)) =
        post_both_raw(&client, &token, PATH, &body("zzzmiddle", false)).await;
    assert_eq!(
        String::from_utf8_lossy(&substring),
        String::from_utf8_lossy(&substring_rs)
    );
    assert_eq!(names(&substring).len(), 1);

    let ((go_status, prefix), (rs_status, prefix_rs)) =
        post_both_raw(&client, &token, PATH, &body("zzzmiddle", true)).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&prefix),
        String::from_utf8_lossy(&prefix_rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        prefix, b"[]\n",
        "nothing *starts* with that, and an empty answer is `[]` not `null`"
    );

    // And the fixture's shared stem does match as a prefix, both of them.
    let stem = f.prefixed.trim_end_matches("aaa").to_owned();
    let ((_, both), (_, both_rs)) = post_both_raw(&client, &token, PATH, &body(&stem, true)).await;
    assert_eq!(
        String::from_utf8_lossy(&both),
        String::from_utf8_lossy(&both_rs)
    );
    let found = names(&both);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.contains(&f.prefixed) && found.contains(&f.middle));
}

/// Every malformed body, and an empty term, are the same 400.
#[tokio::test]
async fn every_bad_body_is_the_same_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for raw in [
        &b"not json"[..],
        &b"[]"[..],
        &b"{\"term\":1}"[..],
        // Decodes to the zero value, so it lands on the empty-term branch — same answer.
        &b"null"[..],
        &b"{}"[..],
        &b"{\"term\":\"\"}"[..],
        &b"{\"prefix_only\":true}"[..],
        &b""[..],
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}] must be rejected by Go");
        assert_eq!(rs_status, go_status, "[{shown}]: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(
            go["id"], "api.context.invalid_body_param.app_error",
            "for the body [{shown}]"
        );
    }
}

/// `json.NewDecoder(...).Decode` reads **one** value and ignores what follows it.
#[tokio::test]
async fn trailing_data_after_the_object_is_ignored() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // `serde_json::from_slice` would call this trailing characters and 400; Go decodes the first
    // object and answers. The port deserializes from a `Deserializer` without `end()` to match.
    let raw = br#"{"term":"zzzmiddle"}{"term":"nonsense"}"#;
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, raw).await;
    assert_eq!(go_status, 200, "Go decodes the first value and stops");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(names(&go), vec![f.middle.clone()]);
}

/// A term matching nothing is an empty array, not a 404.
#[tokio::test]
async fn no_matches_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body("mmrsparitynosuchemojiterm", false),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go, b"[]\n");
    assert_eq!(rs, go);
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
            .json(&serde_json::json!({"term": "x"}))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{PATH}");
    }
}

/// `GET` is Go's method fallthrough to `{emoji_id}`; the rest are forwarded.
#[tokio::test]
async fn a_get_falls_through_to_the_id_route_and_the_rest_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, PATH).await;
    assert_eq!(go_status, 400, "GET {PATH} is `getEmoji` with a bad id");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    // The id is the **url**-param one, not the body-param one this route's own 400s use — the
    // answer comes from `RequireEmojiId`, one route over. `fetch_both_raw` has already asserted
    // that we served it rather than forwarding.
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

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
