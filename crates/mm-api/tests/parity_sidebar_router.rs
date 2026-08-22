//! Router-level parity for `BaseRoutes.ChannelCategories` — the claims that are about
//! *routing* rather than about any handler, and which therefore cannot be checked by reading
//! `sidebar.rs`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity_sidebar_router
//! ```
//!
//! Three separate things are asserted here, each of which has a plausible way of being silently
//! wrong:
//!
//! 1. **The five writes are still forwarded.** Registering *any* method on a path takes that
//!    path out of `Router::fallback`, so a `POST` to a path we serve a `GET` on returns **405
//!    from our router** unless [`crate::partially_migrated`] is used — which is exactly how the
//!    first write route in this project broke a working proxied `GET`. "Still forwarded" is a
//!    claim about the router and is checked by reading `x-mmrs-served-by`.
//!
//! 2. **`order` beats `{category}`.** gorilla decides that by registration order
//!    (`api4/channel.go:80` before `:82`) and axum by specificity. Both land on
//!    `getCategoryOrderForTeamForUser`; if axum had preferred the parameter, `order` would fail
//!    `IsValidCategoryId` and the route would 400 instead of answering an array.
//!
//! 3. **A category segment outside Go's `[A-Za-z0-9_-]+` is forwarded**, so gorilla answers its
//!    own mux 404 with the `detailed_error` that interpolates the request URL, rather than this
//!    server producing a 400 on a request Go never routed.
//!
//! No fixtures: every request here is either refused or forwarded, and the ids are
//! well-formed-but-fictional so nothing can be created or deleted by accident.

mod common;

use common::{GO, RUST, client, go_minted_token, logged_in_user_id, stack_enabled};

/// A syntactically valid id that names nothing. Used where a request would otherwise mutate.
const NOWHERE: &str = "y9i4er48tt8bukijy7i3u5y9ar";

/// Send `method path` to both servers and return each `(status, served_by, body)`.
///
/// Unlike `common::fetch_both_raw` this does **not** assert that Rust served it: whether Rust or
/// Go answered is the thing under test.
async fn send_both(
    client: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> ((u16, String, Vec<u8>), (u16, String, Vec<u8>)) {
    let send = async |base: &str| {
        let mut request = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (
            status,
            served_by,
            response.bytes().await.expect("body reads").to_vec(),
        )
    };

    (send(GO).await, send(RUST).await)
}

/// Compare two bodies that are supposed to be *the same server's* answer to two separate
/// requests.
///
/// `request_id` is minted per request and can never match — it is the one key that differs
/// between Go's direct answer and the same answer arriving through the proxy. Everything else,
/// `message` included, must be identical: unlike
/// `common::assert_error_bodies_match_except_known_gaps` this is Go against Go, so the [D-092]
/// translation gap does not apply.
fn assert_forwarded_bodies_match(go: &[u8], rust: &[u8], context: &str) {
    let (Ok(mut go_value), Ok(mut rust_value)) = (
        serde_json::from_slice::<serde_json::Value>(go),
        serde_json::from_slice::<serde_json::Value>(rust),
    ) else {
        assert_eq!(
            go, rust,
            "{context}: a non-JSON body must match byte for byte"
        );
        return;
    };

    for value in [&mut go_value, &mut rust_value] {
        if let Some(object) = value.as_object_mut() {
            object.remove("request_id");
        }
    }
    assert_eq!(
        go_value, rust_value,
        "{context}: a forwarded body is Go's own"
    );
}

/// Every write on the three paths this file's sibling serves `GET`s on.
///
/// Each body is deliberately one Go rejects — an id that names nothing, or a payload that fails
/// `RequireCategoryId`/`SetInvalidParam` — so that a run of this test creates and deletes
/// nothing even though the requests really do reach Go.
#[tokio::test]
async fn every_write_on_these_paths_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let user_id = logged_in_user_id();
    let base = format!("/api/v4/users/{user_id}/teams/{NOWHERE}/channels/categories");

    let cases: Vec<(reqwest::Method, String, Option<serde_json::Value>)> = vec![
        // createCategoryForTeamForUser
        (
            reqwest::Method::POST,
            base.clone(),
            Some(serde_json::json!({ "display_name": "never created" })),
        ),
        // updateCategoriesForTeamForUser
        (
            reqwest::Method::PUT,
            base.clone(),
            Some(serde_json::json!([])),
        ),
        // updateCategoryOrderForTeamForUser
        (
            reqwest::Method::PUT,
            format!("{base}/order"),
            Some(serde_json::json!([NOWHERE])),
        ),
        // updateCategoryForTeamForUser
        (
            reqwest::Method::PUT,
            format!("{base}/{NOWHERE}"),
            Some(serde_json::json!({ "id": NOWHERE })),
        ),
        // deleteCategoryForTeamForUser
        (reqwest::Method::DELETE, format!("{base}/{NOWHERE}"), None),
    ];

    for (method, path, body) in cases {
        let ((go_status, _, go_body), (rust_status, served_by, rust_body)) =
            send_both(&client, &token, method.clone(), &path, body).await;

        assert_eq!(
            served_by, "go",
            "{method} {path} must still be forwarded, not answered by our router"
        );
        assert_ne!(
            rust_status, 405,
            "{method} {path} returned our router's method-not-allowed — the route is registered \
             without `partially_migrated`"
        );
        assert_eq!(
            go_status, rust_status,
            "{method} {path}: forwarded responses are Go's own"
        );
        assert_forwarded_bodies_match(&go_body, &rust_body, &format!("{method} {path}"));
    }
}

/// The `GET`s that *are* migrated are served locally — the other half of the same claim, and the
/// guard against a route silently failing to register.
#[tokio::test]
async fn the_three_migrated_gets_are_served_locally() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let user_id = logged_in_user_id();
    let base = format!("/api/v4/users/{user_id}/teams/{NOWHERE}/channels/categories");

    for path in [
        base.clone(),
        format!("{base}/order"),
        format!("{base}/{NOWHERE}"),
    ] {
        let (_, (_, served_by, _)) =
            send_both(&client, &token, reqwest::Method::GET, &path, None).await;
        assert_eq!(served_by, "rust", "GET {path} should be ours");
    }
}

/// `order` is a literal beside `{category}` and both routers pick the literal — gorilla because
/// it was registered first, axum because a static segment wins outright.
///
/// The evidence is the *shape of the answer*: `getCategoryOrderForTeamForUser` returns a JSON
/// array, while `getCategoryForTeamForUser` with `category_id = "order"` would fail
/// `IsValidCategoryId` and return a 400 object. So a status of 200 with an array body can only
/// come from the order handler.
#[tokio::test]
async fn the_literal_order_wins_over_the_category_parameter_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let user_id = logged_in_user_id();

    // A team the caller really is in, so the request gets past both permission gates.
    let teams: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/me/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("teams decode");
    let team_id = teams
        .as_array()
        .and_then(|t| t.first())
        .and_then(|t| t["id"].as_str())
        .expect("the fixture user belongs to at least one team");

    let path = format!("/api/v4/users/{user_id}/teams/{team_id}/channels/categories/order");
    let ((go_status, _, go_body), (rust_status, served_by, rust_body)) =
        send_both(&client, &token, reqwest::Method::GET, &path, None).await;

    assert_eq!(served_by, "rust");
    assert_eq!(
        go_status,
        200,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    assert_eq!(go_body, rust_body);

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        parsed.is_array(),
        "an array means the order handler answered; the category handler would have 400'd \
         with an object: {parsed}"
    );

    // And `IsValidCategoryId("order")` really is false, so the fallback would indeed have been a
    // 400 rather than a coincidentally identical answer.
    assert!(!mm_model::sidebar_category::is_valid_category_id("order"));
}

/// A category segment outside `[A-Za-z0-9_-]+` never matched gorilla's route, so Go answers a
/// mux 404 before any handler runs. Forwarded rather than reproduced, so the body — including
/// the `detailed_error` that interpolates the URL — is Go's own with nothing to keep in step.
#[tokio::test]
async fn a_category_segment_outside_gos_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let user_id = logged_in_user_id();

    for segment in ["has.dot", "has%20space", "has~tilde"] {
        let path = format!("/api/v4/users/{user_id}/teams/{NOWHERE}/channels/categories/{segment}");
        let ((go_status, _, go_body), (rust_status, served_by, rust_body)) =
            send_both(&client, &token, reqwest::Method::GET, &path, None).await;

        assert_eq!(served_by, "go", "{path} must be forwarded for Go's mux 404");
        assert_eq!(
            go_status,
            404,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rust_status, go_status);
        assert_forwarded_bodies_match(&go_body, &rust_body, &path);
    }
}

/// A `user_id` or `team_id` outside `[A-Za-z0-9]+` is forwarded by the shared id-charset
/// middleware, exactly as on every other parameterised route. Included here because the
/// category route registers **three** parameters and only two of them are id-shaped — a
/// regression that dropped the middleware would show up here and nowhere else in this family.
#[tokio::test]
async fn an_id_segment_outside_gos_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let user_id = logged_in_user_id();

    for path in [
        format!("/api/v4/users/has-hyphen/teams/{NOWHERE}/channels/categories"),
        format!("/api/v4/users/{user_id}/teams/has-hyphen/channels/categories/order"),
        format!("/api/v4/users/{user_id}/teams/has-hyphen/channels/categories/{NOWHERE}"),
    ] {
        let ((go_status, _, go_body), (rust_status, served_by, rust_body)) =
            send_both(&client, &token, reqwest::Method::GET, &path, None).await;

        assert_eq!(served_by, "go", "{path} must be forwarded");
        assert_eq!(go_status, 404, "{path}");
        assert_eq!(rust_status, go_status);
        assert_forwarded_bodies_match(&go_body, &rust_body, &path);
    }
}
