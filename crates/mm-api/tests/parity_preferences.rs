//! Cross-server parity for `PUT /api/v4/users/me/preferences` — **the first write**.
//!
//! A write has a property a read does not: the other server has to agree afterwards. So these
//! tests write through one server and read back through both.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_preferences
//! ```

mod common;

use common::{GO, RUST, client, go_minted_token, logged_in_user_id, stack_enabled};

const PATH: &str = "/api/v4/users/me/preferences";
const CATEGORY: &str = "display_settings";
// Each test uses its OWN preference name. They run in parallel, and two tests that flip the same
// key race each other — which is a test bug that reads exactly like a cross-server visibility
// failure, so it is worth removing rather than debugging twice.
const NAME: &str = "use_military_time";
const NAME_RUST_WRITE: &str = "mmrs_parity_rust_write";
const NAME_GO_WRITE: &str = "mmrs_parity_go_write";

fn body(name: &str, value: &str) -> serde_json::Value {
    serde_json::json!([{
        "user_id": logged_in_user_id(),
        "category": CATEGORY,
        "name": name,
        "value": value,
    }])
}

async fn put(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    value: &str,
) -> (u16, Vec<u8>) {
    let response = client
        .put(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body(name, value))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    (status, response.bytes().await.expect("body").to_vec())
}

/// Read the preference back through a given server.
async fn read_back(client: &reqwest::Client, base: &str, token: &str, name: &str) -> String {
    let response = client
        .get(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("readable");
    let prefs: Vec<serde_json::Value> = response.json().await.expect("decodes");
    prefs
        .into_iter()
        .find(|p| p["name"] == name)
        .map(|p| p["value"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_else(|| "<absent>".to_owned())
}

/// The success body is Go's `ReturnStatusOK` — `{"status":"OK"}`, no newline.
#[tokio::test]
async fn the_success_body_is_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let (go_status, go_body) = put(&client, GO, &token, NAME, "true").await;
    let (rs_status, rs_body) = put(&client, RUST, &token, NAME, "true").await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the OK body must match Go's ReturnStatusOK exactly"
    );
    assert_ne!(rs_body.last(), Some(&b'\n'), "w.Write appends no newline");
}

/// **The point of migrating a write.** A value written through Rust must be visible to the Go
/// server, which is what makes the two servers usable at once.
///
/// Measured rather than assumed: this is where [D-087]'s stale-on-write would show up if
/// preferences were cached the way users are. They are not — Go reflects the write immediately.
#[tokio::test]
async fn a_write_through_rust_is_visible_to_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    // Write a value the other direction from whatever is there, so a no-op cannot pass.
    let before = read_back(&client, GO, &token, NAME_RUST_WRITE).await;
    let target = if before == "true" { "false" } else { "true" };

    let (status, _) = put(&client, RUST, &token, NAME_RUST_WRITE, target).await;
    assert_eq!(status, 200);

    assert_eq!(
        read_back(&client, GO, &token, NAME_RUST_WRITE).await,
        target,
        "the Go server must see a write made through Rust"
    );
    assert_eq!(
        read_back(&client, RUST, &token, NAME_RUST_WRITE).await,
        target
    );
}

/// And the reverse direction, which is the one the Strangler Fig depends on for every route that
/// has *not* been migrated.
#[tokio::test]
async fn a_write_through_go_is_visible_to_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let before = read_back(&client, RUST, &token, NAME_GO_WRITE).await;
    let target = if before == "true" { "false" } else { "true" };

    let (status, _) = put(&client, GO, &token, NAME_GO_WRITE, target).await;
    assert_eq!(status, 200);

    assert_eq!(
        read_back(&client, RUST, &token, NAME_GO_WRITE).await,
        target,
        "we must see a write made through the Go server"
    );
}

/// A foreign `user_id` in the body is 403 on both servers, and the error bodies agree on
/// everything except the translated message.
#[tokio::test]
async fn a_foreign_user_id_is_rejected_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let foreign = serde_json::json!([{
        "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "category": CATEGORY,
        "name": NAME,
        "value": "true",
    }]);

    let send = async |base: &str| {
        let response = client
            .put(format!("{base}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&foreign)
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("decodes");
        (status, body)
    };

    let (go_status, go_body) = send(GO).await;
    let (rs_status, rs_body) = send(RUST).await;

    assert_eq!(go_status, 403);
    assert_eq!(
        rs_status, 403,
        "a foreign user id must be forbidden here too"
    );
    assert_eq!(
        rs_body["id"], go_body["id"],
        "the error id is what clients branch on"
    );
    assert_eq!(rs_body["status_code"], go_body["status_code"]);

    // `detailed_error` is wiped by Go unless developer mode is on, and we reproduce that
    // unconditionally — so it must be empty on both sides, not merely present on both.
    assert_eq!(rs_body["detailed_error"], "");
    assert_eq!(go_body["detailed_error"], "");

    // Same key set, so a field cannot appear on one side only.
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&rs_body), keys(&go_body));

    assert!(
        !rs_body["request_id"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "every error carries a request id, as Go's does"
    );

    // The one field that legitimately differs: Go translates the id through its i18n bundle and
    // we emit the id itself. See D-092.
    assert_eq!(rs_body["message"], rs_body["id"]);
    assert_ne!(go_body["message"], go_body["id"]);
}

/// Both of Go's batch bounds, checked against Go itself.
#[tokio::test]
async fn the_batch_bounds_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let send = async |base: &str, payload: serde_json::Value| {
        client
            .put(format!("{base}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&payload)
            .send()
            .await
            .expect("reachable")
            .status()
            .as_u16()
    };

    // Empty is an error, not a successful no-op.
    let empty = serde_json::json!([]);
    assert_eq!(send(RUST, empty.clone()).await, send(GO, empty).await);

    // 101 entries is over `maxUpdatePreferences`.
    let too_many: Vec<serde_json::Value> = (0..101)
        .map(|i| {
            serde_json::json!({
                "user_id": logged_in_user_id(), "category": CATEGORY,
                "name": format!("bulk_{i}"), "value": "1",
            })
        })
        .collect();
    let too_many = serde_json::Value::Array(too_many);
    assert_eq!(
        send(RUST, too_many.clone()).await,
        send(GO, too_many).await,
        "the 100-entry cap must be enforced on both sides"
    );
}

/// The route is served here, not forwarded — unless it touches `flagged_post`.
#[tokio::test]
async fn ordinary_categories_are_served_by_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let response = client
        .put(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body(NAME, "true"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust")
    );
}

/// A path with one migrated method must still forward the others.
///
/// axum matches the path before the method, so registering `PUT` here made `GET` return 405 from
/// our own router instead of reaching the proxy — breaking a route that had been working. This is
/// the regression test for that, and it belongs on every partially migrated path.
#[tokio::test]
async fn an_unmigrated_method_on_a_migrated_path_still_reaches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let response = client
        .get(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response.status(),
        200,
        "GET is not migrated, so it must be proxied — 405 here means the method fallback is gone"
    );
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "and it must be Go that answered"
    );
}
