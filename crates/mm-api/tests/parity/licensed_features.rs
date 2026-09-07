//! Cross-server parity for the seventeen routes whose **first statement** is a licence test:
//! all of `content_flagging.go`, and the four write halves of `channel_bookmark.go`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity licensed_features
//! ```
//!
//! # The assertion that matters is that nothing else is consulted
//!
//! These are not "a refusal after the usual checks" like `/data_retention` — the licence test is
//! the first line of every handler, so a caller with **no permission**, a **malformed body** and a
//! **non-existent id** still gets the 501 and nothing else. Each of those three is asserted
//! separately, because each is a check a reader might add for symmetry with a neighbouring file
//! and every one of them would be a divergence on every request.

use crate::common;

use common::{ACTIVE_LICENCE_ROW, GO, RUST, client, go_minted_token, stack_enabled};

const FLAGGING_ERROR: &str = "api.data_spillage.error.license";
const BOOKMARK_ERROR: &str = "api.channel.bookmark.channel_bookmark.license.error";

/// Ids of the right shape that are no object.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// Every migrated route, as `(method, path, error id)`.
fn routes() -> Vec<(reqwest::Method, String, &'static str)> {
    let m = |s: &str| reqwest::Method::from_bytes(s.as_bytes()).expect("a method");
    vec![
        (
            m("GET"),
            "/api/v4/content_flagging/flag/config".into(),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            "/api/v4/content_flagging/fields".into(),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            "/api/v4/content_flagging/config".into(),
            FLAGGING_ERROR,
        ),
        (
            m("PUT"),
            "/api/v4/content_flagging/config".into(),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/content_flagging/team/{NOWHERE}/status"),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/content_flagging/team/{NOWHERE}/reviewers/search"),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/content_flagging/post/{NOWHERE}"),
            FLAGGING_ERROR,
        ),
        (
            m("POST"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/flag"),
            FLAGGING_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/field_values"),
            FLAGGING_ERROR,
        ),
        (
            m("PUT"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/remove"),
            FLAGGING_ERROR,
        ),
        (
            m("PUT"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/keep"),
            FLAGGING_ERROR,
        ),
        (
            m("POST"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/report"),
            FLAGGING_ERROR,
        ),
        (
            m("POST"),
            format!("/api/v4/content_flagging/post/{NOWHERE}/assign/{NOWHERE}"),
            FLAGGING_ERROR,
        ),
        (
            m("POST"),
            format!("/api/v4/channels/{NOWHERE}/bookmarks"),
            BOOKMARK_ERROR,
        ),
        (
            m("PATCH"),
            format!("/api/v4/channels/{NOWHERE}/bookmarks/{NOWHERE}"),
            BOOKMARK_ERROR,
        ),
        (
            m("DELETE"),
            format!("/api/v4/channels/{NOWHERE}/bookmarks/{NOWHERE}"),
            BOOKMARK_ERROR,
        ),
        (
            m("POST"),
            format!("/api/v4/channels/{NOWHERE}/bookmarks/{NOWHERE}/sort_order"),
            BOOKMARK_ERROR,
        ),
    ]
}

/// Send one request to both servers and assert they agree.
async fn both(
    client: &reqwest::Client,
    token: &str,
    method: &reqwest::Method,
    path: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let call = async |base: &str| {
        let response = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            status,
            served_by,
            response.bytes().await.expect("reads").to_vec(),
        )
    };

    let (go_status, _, go) = call(GO).await;
    let (rs_status, served_by, rs) = call(RUST).await;
    assert_eq!(served_by.as_deref(), Some("rust"), "{method} {path}");
    assert_eq!(rs_status, go_status, "{method} {path}");
    let parsed =
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, &format!("{method} {path}"));
    (go_status, parsed)
}

/// All seventeen, as an administrator with a well-formed body.
#[tokio::test]
async fn every_route_is_its_families_licence_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = routes();
    assert_eq!(
        all.len(),
        17,
        "thirteen content-flagging and four bookmark writes"
    );

    for (method, path, id) in &all {
        let (status, parsed) = both(&client, &token, method, path, b"{}").await;
        assert_eq!(status, 501, "{method} {path}");
        assert_eq!(parsed["id"], *id, "{method} {path}");
    }

    // The two families really do use different ids, so the loop above is not asserting one
    // constant seventeen times.
    assert_ne!(FLAGGING_ERROR, BOOKMARK_ERROR);
}

/// **Nothing else is consulted.** A malformed body, on every route that has one.
#[tokio::test]
async fn a_malformed_body_does_not_reach_a_decoder() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path, id) in routes() {
        if method == reqwest::Method::GET {
            continue;
        }
        let (status, parsed) = both(&client, &token, &method, &path, b"{").await;
        assert_eq!(
            status, 501,
            "the licence test is the first statement, so the body is never decoded: {method} {path}"
        );
        assert_eq!(parsed["id"], id);
    }
}

/// **Nothing else is consulted.** A caller with no permission at all.
#[tokio::test]
async fn a_plain_user_gets_the_same_refusal_as_an_admin() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "licfeat").await;
    let plain = common::create_plain_user(&client, &admin, &team, "licfeat").await;

    for (method, path, id) in routes() {
        let (status, parsed) = both(&client, &plain.token, &method, &path, b"{}").await;
        assert_eq!(
            status, 501,
            "no permission is checked ahead of the refusal: {method} {path}"
        );
        assert_eq!(parsed["id"], id);
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **Nothing else is consulted.** A *real* channel the caller is a member of gives the same answer
/// as one that does not exist — so the refusal is not accidentally coming from a missing object.
#[tokio::test]
async fn a_real_channel_gives_the_same_refusal_as_a_missing_one() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let channel = common::a_channel_the_user_is_in(&client, &token).await;

    let real = format!("/api/v4/channels/{channel}/bookmarks");
    let (status, parsed) = both(&client, &token, &reqwest::Method::POST, &real, b"{}").await;
    assert_eq!(status, 501);
    assert_eq!(parsed["id"], BOOKMARK_ERROR);

    let missing = format!("/api/v4/channels/{NOWHERE}/bookmarks");
    let (missing_status, missing_parsed) =
        both(&client, &token, &reqwest::Method::POST, &missing, b"{}").await;
    assert_eq!(
        (missing_status, &missing_parsed["id"]),
        (status, &parsed["id"]),
        "a real channel and a missing one are the same answer, because neither is looked up"
    );
}

/// **All five bookmark routes share the gate**, including the `GET` this suite first assumed was
/// an ordinary read.
///
/// `listChannelBookmarksForChannel` opens with the same `License() == nil` test as the four
/// writes. It was already ported — in `channels.rs`, beside two other channel routes with the same
/// shape — so the assertion here is that the five agree, which is what says the four new ones were
/// given the right error id rather than a plausible one.
#[tokio::test]
async fn listing_bookmarks_is_the_same_refusal_as_writing_one() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let channel = common::a_channel_the_user_is_in(&client, &token).await;
    let path = format!("/api/v4/channels/{channel}/bookmarks");

    let (get_status, get_body) = both(&client, &token, &reqwest::Method::GET, &path, b"").await;
    assert_eq!(get_status, 501, "the list is licence-gated too");
    assert_eq!(get_body["id"], BOOKMARK_ERROR);

    let (post_status, post_body) =
        both(&client, &token, &reqwest::Method::POST, &path, b"{}").await;
    assert_eq!(
        (post_status, &post_body["id"]),
        (get_status, &get_body["id"]),
        "reading and writing a bookmark are the same refusal, on the same path"
    );
}

/// **A licence row hands every one of them back to Go.**
///
/// Added because a mutation making the licensed branch answer 501 as well — that is, never
/// forwarding — **survived** the first run: every test here runs unlicensed, so "refuse" and
/// "refuse or forward" are the same program. The work behind these gates is not ported and never
/// will be from this side, so answering a licensed server ourselves would be silently wrong on a
/// deployment that has a licence.
///
/// Holds the shared lock exclusively, like the other licence-flipping tests.
#[tokio::test]
async fn a_licence_row_hands_every_route_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |method: &reqwest::Method, path: &str| -> Option<String> {
        client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(b"{}".to_vec())
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    let all = routes();

    common::set_active_licence_id(None).await;
    for (method, path, _) in &all {
        assert_eq!(
            served_by(method, path).await.as_deref(),
            Some("rust"),
            "unlicensed, {method} {path} is ours"
        );
    }

    common::set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut forwarded = Vec::new();
    for (method, path, _) in &all {
        forwarded.push((format!("{method} {path}"), served_by(method, path).await));
    }

    // Restore before asserting, so a failure does not leave the stack licensed for every other
    // suite in this binary.
    common::set_active_licence_id(None).await;
    let mut cleared = Vec::new();
    for (method, path, _) in &all {
        cleared.push((format!("{method} {path}"), served_by(method, path).await));
    }

    for (route, served) in &forwarded {
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "licensed, {route} must be forwarded — the feature behind the gate is not ported"
        );
    }
    for (route, served) in &cleared {
        assert_eq!(
            served.as_deref(),
            Some("rust"),
            "and it comes back: {route}"
        );
    }
}

/// Registering seventeen methods must not turn their neighbours into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (
            reqwest::Method::DELETE,
            "/api/v4/content_flagging/config".to_owned(),
        ),
        (
            reqwest::Method::POST,
            "/api/v4/content_flagging/fields".to_owned(),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/content_flagging/post/{NOWHERE}"),
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/channels/{NOWHERE}/bookmarks/{NOWHERE}"),
        ),
    ] {
        let ours = client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {path} must be forwarded"
        );
    }
}
