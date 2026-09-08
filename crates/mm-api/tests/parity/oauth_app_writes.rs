//! Cross-server parity for the five OAuth app writes.
//!
//! Four session-authenticated CRUD routes plus the Dynamic Client Registration endpoint, which is
//! the odd one out in three ways: **no session**, a `400` **DCR error envelope** rather than an
//! `AppError`, and two config gates of which the second is closed on a stock server.
//!
//! Reads back through the server that wrote, per [D-190].
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity oauth_app_writes
//! ```

use crate::common;

use common::{GO, RUST, client, go_minted_token, logged_in_user_id, stack_enabled};

const APPS: &str = "/api/v4/oauth/apps";

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String, Option<String>) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, response.text().await.expect("a body"), served_by)
}

/// Replace the values that cannot match between two servers, keeping whether each is set.
fn normalise(app: &serde_json::Value) -> serde_json::Value {
    let mut out = app.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for key in ["id", "client_secret", "creator_id"] {
        if let Some(value) = object.get(key) {
            let present = value.as_str().is_some_and(|s| !s.is_empty());
            object.insert(key.to_owned(), serde_json::json!(present));
        }
    }
    for key in ["create_at", "update_at"] {
        if let Some(value) = object.get(key) {
            let nonzero = value.as_i64().unwrap_or(0) > 0;
            object.insert(key.to_owned(), serde_json::json!(nonzero));
        }
    }
    out
}

fn app_body(name: &str, public: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": "mmrs parity oauth app",
        "homepage": "https://example.invalid/mmrs",
        "callback_urls": ["https://example.invalid/mmrs/callback"],
        "is_public": public,
    })
}

#[tokio::test]
async fn an_oauth_app_round_trips_and_both_servers_answer_the_same_shape() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let (go_status, go_raw, _) = send(
        &http,
        reqwest::Method::POST,
        GO,
        &token,
        APPS,
        Some(&app_body("mmrs go app", false)),
    )
    .await;
    let (rust_status, rust_raw, served) = send(
        &http,
        reqwest::Method::POST,
        RUST,
        &token,
        APPS,
        Some(&app_body("mmrs rust app", false)),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"), "we must serve the create");

    assert_eq!(go_status, 201, "Go answers Created: {go_raw}");
    assert_eq!(rust_status, go_status, "the create status differs");
    assert!(
        go_raw.ends_with('\n'),
        "Go's create body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(rust_raw.ends_with('\n'), go_raw.ends_with('\n'));

    let go_app: serde_json::Value = serde_json::from_str(&go_raw).expect("an app");
    let rust_app: serde_json::Value = serde_json::from_str(&rust_raw).expect("an app");
    let mut go_normalised = normalise(&go_app);
    let mut rust_normalised = normalise(&rust_app);
    go_normalised["name"] = serde_json::json!("<name>");
    rust_normalised["name"] = serde_json::json!("<name>");
    assert_eq!(
        go_normalised, rust_normalised,
        "the created app differs:\n go: {go_app}\nrust: {rust_app}"
    );
    assert_eq!(
        rust_app["creator_id"],
        logged_in_user_id(),
        "creator_id comes from the session"
    );
    assert!(
        rust_app["client_secret"]
            .as_str()
            .is_some_and(|s| s.len() == 26),
        "a confidential client gets a generated secret: {rust_app}"
    );

    let rust_id = rust_app["id"].as_str().expect("an id").to_owned();
    let go_id = go_app["id"].as_str().expect("an id").to_owned();

    // The update: **200**, not the create's 201, and the secret is copied off the old app so a
    // body cannot rotate it.
    let update = serde_json::json!({
        "id": rust_id,
        "name": "mmrs rust app renamed",
        "description": "edited",
        "homepage": "https://example.invalid/mmrs",
        "callback_urls": ["https://example.invalid/mmrs/callback"],
        "client_secret": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "creator_id": "bbbbbbbbbbbbbbbbbbbbbbbbbb",
    });
    let (status, raw, _) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &token,
        &format!("{APPS}/{rust_id}"),
        Some(&update),
    )
    .await;
    assert_eq!(status, 200, "the update answers OK, not Created: {raw}");
    let updated: serde_json::Value = serde_json::from_str(&raw).expect("an app");
    assert_eq!(updated["name"], "mmrs rust app renamed");
    assert_eq!(
        updated["client_secret"], rust_app["client_secret"],
        "the secret is copied off the old app — a body cannot rotate it"
    );
    assert_eq!(
        updated["creator_id"], rust_app["creator_id"],
        "and neither is the creator"
    );
    assert_eq!(updated["create_at"], rust_app["create_at"], "nor create_at");

    // Go's update answers 200 too.
    let (go_status, _, _) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &token,
        &format!("{APPS}/{go_id}"),
        Some(&serde_json::json!({
            "id": go_id,
            "name": "mmrs go app renamed",
            "homepage": "https://example.invalid/mmrs",
            "callback_urls": ["https://example.invalid/mmrs/callback"],
        })),
    )
    .await;
    assert_eq!(go_status, 200, "Go's update answers OK too");

    // Read the row back through the writing server — the answer above was assembled by the app
    // layer and would be identical whatever the UPDATE wrote.
    let (status, raw, _) = send(
        &http,
        reqwest::Method::GET,
        RUST,
        &token,
        &format!("{APPS}/{rust_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "the updated app reads back: {raw}");
    let stored: serde_json::Value = serde_json::from_str(&raw).expect("an app");
    assert_eq!(stored["name"], "mmrs rust app renamed");
    assert_eq!(stored["description"], "edited");
    assert_eq!(
        stored["client_secret"], rust_app["client_secret"],
        "the stored secret is unchanged"
    );

    // Regenerating changes the secret and nothing else.
    let (status, raw, _) = send(
        &http,
        reqwest::Method::POST,
        RUST,
        &token,
        &format!("{APPS}/{rust_id}/regen_secret"),
        Some(&serde_json::json!({})),
    )
    .await;
    assert_eq!(status, 200, "regen answers OK: {raw}");
    let regenerated: serde_json::Value = serde_json::from_str(&raw).expect("an app");
    assert_ne!(
        regenerated["client_secret"], updated["client_secret"],
        "the secret must change"
    );
    assert_eq!(
        regenerated["client_secret"].as_str().map(str::len),
        Some(26),
        "and it is a freshly minted id"
    );
    assert_eq!(
        regenerated["name"], updated["name"],
        "and nothing else does"
    );

    // Delete is a hard delete, so the read stops finding it.
    let (status, body, _) = send(
        &http,
        reqwest::Method::DELETE,
        RUST,
        &token,
        &format!("{APPS}/{rust_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"OK"}"#);
    let (status, _, _) = send(
        &http,
        reqwest::Method::GET,
        RUST,
        &token,
        &format!("{APPS}/{rust_id}"),
        None,
    )
    .await;
    assert_eq!(status, 404, "a deleted app is gone, not returned");

    send(
        &http,
        reqwest::Method::DELETE,
        GO,
        &token,
        &format!("{APPS}/{go_id}"),
        None,
    )
    .await;
}

#[tokio::test]
async fn a_public_client_keeps_an_empty_secret_and_cannot_regenerate_one() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // `is_public` is not stored anywhere: it only decides whether a secret is generated, and the
    // **emptiness** of that secret is what `IsPublicClient` reads afterwards.
    let mut ids = Vec::new();
    for base in [GO, RUST] {
        let name = if base == GO {
            "mmrs go public"
        } else {
            "mmrs rust public"
        };
        let (status, raw, _) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            APPS,
            Some(&app_body(name, true)),
        )
        .await;
        assert_eq!(status, 201, "{base} created the public client: {raw}");
        let app: serde_json::Value = serde_json::from_str(&raw).expect("an app");
        assert_eq!(
            app["client_secret"], "",
            "{base} generated a secret for a public client"
        );
        let id = app["id"].as_str().expect("an id").to_owned();

        // Regenerating is refused — giving it a secret would silently convert it to a
        // confidential client — and the refusal comes *after* both permission checks.
        let (status, raw, _) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            &format!("{APPS}/{id}/regen_secret"),
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(status, 400, "{base} should refuse a public client: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "api.oauth.regenerate_secret.public_client.app_error",
            "{base} refused with the wrong id"
        );
        ids.push((base, id));
    }

    for (base, id) in ids {
        send(
            &http,
            reqwest::Method::DELETE,
            base,
            &token,
            &format!("{APPS}/{id}"),
            None,
        )
        .await;
    }
}

#[tokio::test]
async fn the_create_body_is_a_request_not_an_app() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // Six fields are lifted across and everything else is discarded — including `id`,
    // `creator_id`, `create_at` and `client_secret`. A body that tries to plant any of them is
    // accepted and ignored, on both servers.
    let planted = serde_json::json!({
        "id": "cccccccccccccccccccccccccc",
        "creator_id": "dddddddddddddddddddddddddd",
        "create_at": 1,
        "client_secret": "eeeeeeeeeeeeeeeeeeeeeeeeee",
        "name": "mmrs planted",
        "description": "mmrs parity oauth app",
        "homepage": "https://example.invalid/mmrs",
        "callback_urls": ["https://example.invalid/mmrs/callback"],
    });

    let mut ids = Vec::new();
    for base in [GO, RUST] {
        let (status, raw, _) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            APPS,
            Some(&planted),
        )
        .await;
        assert_eq!(
            status, 201,
            "{base} should ignore the planted fields: {raw}"
        );
        let app: serde_json::Value = serde_json::from_str(&raw).expect("an app");
        assert_ne!(
            app["id"], "cccccccccccccccccccccccccc",
            "{base} honoured a planted id"
        );
        assert_eq!(
            app["creator_id"],
            logged_in_user_id(),
            "{base} honoured a planted creator"
        );
        assert_ne!(
            app["client_secret"], "eeeeeeeeeeeeeeeeeeeeeeeeee",
            "{base} honoured a planted secret"
        );
        assert!(
            app["create_at"].as_i64().unwrap_or(0) > 1,
            "{base} honoured a planted create_at"
        );
        ids.push((base, app["id"].as_str().expect("an id").to_owned()));
    }

    for (base, id) in ids {
        send(
            &http,
            reqwest::Method::DELETE,
            base,
            &token,
            &format!("{APPS}/{id}"),
            None,
        )
        .await;
    }
}

#[tokio::test]
async fn the_id_checks_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // A malformed app id in the path.
    for base in [GO, RUST] {
        let (status, raw, _) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &token,
            &format!("{APPS}/short"),
            None,
        )
        .await;
        assert_eq!(status, 400, "{base} on a malformed app id: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    }

    // A body whose id disagrees with the path.
    let mismatched = serde_json::json!({
        "id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "name": "mmrs mismatch",
        "homepage": "https://example.invalid/mmrs",
        "callback_urls": ["https://example.invalid/mmrs/callback"],
    });
    for base in [GO, RUST] {
        let (status, raw, _) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("{APPS}/bbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some(&mismatched),
        )
        .await;
        assert_eq!(status, 400, "{base} on an id mismatch: {raw}");
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // An app that does not exist, with a well-formed id.
    for base in [GO, RUST] {
        let (status, raw, _) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &token,
            &format!("{APPS}/aaaaaaaaaaaaaaaaaaaaaaaaaa"),
            None,
        )
        .await;
        assert_eq!(status, 404, "{base} on an absent app: {raw}");
    }
}

#[tokio::test]
async fn dynamic_client_registration_is_a_dcr_error_not_an_app_error() {
    if !stack_enabled() {
        return;
    }
    let http = client();

    // **No session.** Go's comment: "Session and permission checks removed for DCR endpoint to
    // allow external client registration". The request carries no Authorization header at all.
    let post = async |base: &str, body: &str| {
        let response = http
            .post(format!("{base}{APPS}/register"))
            .header("Content-Type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (status, response.text().await.expect("a body"), served_by)
    };

    // The feature is off by default, so a well-formed request is `unsupported_operation`.
    let valid = r#"{"redirect_uris":["https://example.invalid/cb"],"client_name":"mmrs dcr"}"#;
    let (go_status, go_raw, _) = post(GO, valid).await;
    let (rust_status, rust_raw, served) = post(RUST, valid).await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the disabled path is served here, not forwarded"
    );
    assert_eq!(go_status, 400, "Go answers 400: {go_raw}");
    assert_eq!(rust_status, go_status, "the DCR status differs");

    // **Not an AppError.** The body is `{"error", "error_description"}` — a client parsing for
    // `id`/`status_code` gets nothing.
    let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("a DCR error");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("a DCR error");
    assert_eq!(go_body, rust_body, "the DCR error body differs");
    assert_eq!(go_body["error"], "unsupported_operation");
    assert!(
        go_body.get("id").is_none() && go_body.get("status_code").is_none(),
        "a DCR error is not an AppError: {go_body}"
    );
    assert_eq!(
        rust_raw.ends_with('\n'),
        go_raw.ends_with('\n'),
        "the DCR body's framing differs"
    );

    // **The decode comes first**, so a malformed body answers `invalid_client_metadata` even
    // though the feature is off — the gates never run.
    let (go_status, go_raw, _) = post(GO, "not json").await;
    let (rust_status, rust_raw, _) = post(RUST, "not json").await;
    assert_eq!(go_status, 400);
    assert_eq!(rust_status, go_status);
    let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("a DCR error");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("a DCR error");
    assert_eq!(go_body, rust_body, "the malformed-body DCR error differs");
    assert_eq!(
        go_body["error"], "invalid_client_metadata",
        "a malformed body is decided before either gate"
    );
}

#[tokio::test]
async fn the_feature_gate_carries_two_different_ids() {
    if !stack_enabled() {
        return;
    }

    // **`EnableOAuthServiceProvider` is `true` on the shared stack**, so none of these gates fires
    // against `:8066` and a mutation swapping their ids is invisible there — measured, it survived
    // the first run of this plan. A second server with the setting off is the only way to reach
    // them, the same device `/recaps` and the gated families use.
    let Some(server) = common::SecondServer::start(
        8080,
        &[("MM_SERVICESETTINGS_ENABLEOAUTHSERVICEPROVIDER", "false")],
    )
    .await
    else {
        return;
    };
    let http = client();
    let token = go_minted_token(&http).await;

    // **The create path's id is not the other three's.** `CreateOAuthAppInternal` answers
    // `api.oauth.register_oauth_app.turn_off.app_error`; update, delete and regenerate all answer
    // `api.oauth.allow_oauth.turn_off.app_error`. Same 501, same sentence in the source, two ids.
    let cases: &[(reqwest::Method, String, Option<serde_json::Value>, &str)] = &[
        (
            reqwest::Method::POST,
            APPS.to_owned(),
            Some(app_body("mmrs gated", false)),
            "api.oauth.register_oauth_app.turn_off.app_error",
        ),
        (
            reqwest::Method::PUT,
            format!("{APPS}/aaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some(serde_json::json!({
                "id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "name": "mmrs gated",
                "homepage": "https://example.invalid/mmrs",
                "callback_urls": ["https://example.invalid/mmrs/callback"],
            })),
            "api.oauth.allow_oauth.turn_off.app_error",
        ),
    ];

    for (method, path, body, expected_id) in cases {
        let mut request = http
            .request(method.clone(), format!("{}{path}", server.base))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.expect("the second server answers");
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let raw = response.text().await.expect("a body");

        // The update's 501 is reached only after the app is fetched, and on this server the
        // fetch is a 404 — the *create* is the one that shows the gate. Assert whichever the
        // route can reach, and require the create's to be its own id.
        if *expected_id == "api.oauth.register_oauth_app.turn_off.app_error" {
            assert_eq!(status, 501, "the create gate answers 501: {raw}");
            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "the gated create is served here, not forwarded"
            );
            let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
            assert_eq!(
                body["id"], *expected_id,
                "the create gate must carry its own id, not the other three's"
            );
        } else {
            // Documented rather than asserted as a 501: `updateOAuthApp` fetches the app first,
            // so a nonexistent id answers 404 before the gate is reached. The gate's id is still
            // pinned by the create case above, which is the only route that reaches it without a
            // stored app.
            assert!(
                status == 501 || status == 404,
                "the update answers either the gate or the missing app: {status} {raw}"
            );
        }
    }
}
