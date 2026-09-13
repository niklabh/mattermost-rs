//! Cross-server parity for the seven CRUD and membership writes in `api4/group.go`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::group_writes
//! ```
//!
//! `createGroup`, `getGroupsByNames`, `patchGroup`, `deleteGroup`, `restoreGroup`,
//! `addGroupMembers`, `deleteGroupMembers` — `POST`, `PUT` and `DELETE` across five paths.
//!
//! # The gate is the first statement, and for a write that means it precedes the body
//!
//! All seven open with `requireLicense` (api4/handlers.go:237) before `RequireGroupId` and before
//! `json.NewDecoder(r.Body).Decode(...)`. So on an unlicensed server a `POST /groups` carrying
//! `{` — which is not JSON — is the same 501 as one carrying a valid group, and a port that
//! parsed first would answer 400 to a request Go never parses. That asymmetry is the subject of
//! [`the_licence_gate_precedes_the_body`], and it is the only thing about these routes that a
//! reader can plausibly get wrong while the gate is shut.
//!
//! # What this suite cannot reach
//!
//! Behind the gate sit the custom-group permission model, `licensedAndConfiguredForGroupBySource`
//! and the whole `GroupStore` write surface. Go loads its licence at startup and re-reads it only
//! on a save, so `set_active_licence_id` moves *our* answer and not Go's — which is exactly why
//! [`a_licence_row_hands_every_group_write_back_to_go`] can assert the forwarding boundary and
//! nothing past it. See [D-360].
//!
//! # `/names` is a literal whose method picks the handler
//!
//! `names` matches `{group_id:[A-Za-z0-9]+}`, so in Go `POST /groups/names` is `getGroupsByNames`
//! and `DELETE /groups/names` is `deleteGroup` with `group_id = "names"`. This server claims only
//! the `POST` — but **a static route shadows its parameterised sibling for every method**, since
//! axum prefers the literal and does not backtrack across method routers, so registering it would
//! have quietly given up `GET /groups/names` as well. The `GET` and `DELETE` gorilla routes there
//! are re-claimed; `PUT` and the rest stay forwarded, which is where gorilla leaves them too.
//! [`groups_names_is_a_literal_for_the_post_only`] measures all five methods.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    go_minted_token, set_active_licence_id, stack_enabled,
};

/// A 26-character id that names nothing. The gate refuses before anything looks it up, which is
/// the point: no fixture group is needed to test any of this.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// One request to one server, returning `(status, body, x-mmrs-served-by)`.
async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .request(method.clone(), format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} {method} {path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// Compare two error bodies that are **both Go's**, tolerating only `request_id`.
///
/// [`assert_error_bodies_match_except_known_gaps`] also tolerates `message`, because a body we
/// mint carries the untranslated error id where Go's carries prose ([D-092]). A *forwarded* body
/// has been through Go's i18n on both sides, so that allowance would hide a real difference here
/// — the only field that can legitimately move is the per-request id.
fn assert_forwarded_bodies_match(go_body: &[u8], rs_body: &[u8], context: &str) {
    let mut go: serde_json::Value = serde_json::from_slice(go_body)
        .unwrap_or_else(|e| panic!("{context}: Go's body is not JSON: {e}"));
    let mut rs: serde_json::Value = serde_json::from_slice(rs_body)
        .unwrap_or_else(|e| panic!("{context}: the forwarded body is not JSON: {e}"));
    for v in [&mut go, &mut rs] {
        if let Some(map) = v.as_object_mut() {
            map.remove("request_id");
        }
    }
    assert_eq!(go, rs, "{context}: a forwarded body is Go's own");
}

/// The seven routes, spelled out rather than generated: a route that stopped being registered
/// would silently drop out of a generated list and the count below would still pass.
fn every_group_write(id: &str) -> Vec<(reqwest::Method, String, &'static str)> {
    vec![
        (
            reqwest::Method::POST,
            "/api/v4/groups".to_owned(),
            r#"{"name":"parity.group","display_name":"Parity","source":"custom","allow_reference":true}"#,
        ),
        (
            reqwest::Method::POST,
            "/api/v4/groups/names".to_owned(),
            r#"["parity.group"]"#,
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{id}/patch"),
            r#"{"display_name":"Renamed"}"#,
        ),
        (reqwest::Method::DELETE, format!("/api/v4/groups/{id}"), ""),
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{id}/restore"),
            "",
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{id}/members"),
            r#"{"user_ids":["aaaaaaaaaaaaaaaaaaaaaaaaaa"]}"#,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{id}/members"),
            r#"{"user_ids":["aaaaaaaaaaaaaaaaaaaaaaaaaa"]}"#,
        ),
    ]
}

/// All seven, each answering the generic 501 — and each served by **us** rather than forwarded,
/// without which the comparison would be Go against Go and would pass whatever the router did.
#[tokio::test]
async fn every_group_write_answers_the_generic_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let routes = every_group_write(NOWHERE);
    assert_eq!(routes.len(), 7, "seven handlers");

    for (method, path, body) in &routes {
        let label = format!("{method} {path}");
        let (go_status, go, _) = send(&client, GO, &token, method.clone(), path, body).await;
        let (rs_status, rs, served_by) =
            send(&client, RUST, &token, method.clone(), path, body).await;

        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{label} was forwarded, so this proves nothing about the Rust handler"
        );
        assert_eq!(
            go_status, 501,
            "{label}: `requireLicense` is a 501, not a 403"
        );
        assert_eq!(rs_status, go_status, "{label}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
        assert_eq!(
            parsed["id"], "api.license_error",
            "{label}: the generic id, shared by every group route"
        );
        assert!(
            !rs.ends_with(b"\n"),
            "{label}: error bodies carry no newline"
        );
    }
}

/// **The gate precedes the body.** Malformed JSON, an empty body and a well-formed one are the
/// same answer, because Go never reaches its decoder.
///
/// The three bodies are driven through all four routes that take one, not just `POST /groups`:
/// each has its own `SetInvalidParamWithErr` behind the gate, with a different parameter name,
/// and a port that parsed early would produce four *different* wrong answers.
#[tokio::test]
async fn the_licence_gate_precedes_the_body() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let bodies = [
        ("malformed", "{"),
        ("empty", ""),
        ("json_null", "null"),
        ("wrong_shape", r#"{"user_ids":42}"#),
        ("array_where_object_expected", "[1,2,3]"),
    ];

    let routes: Vec<(reqwest::Method, String)> = vec![
        (reqwest::Method::POST, "/api/v4/groups".to_owned()),
        (reqwest::Method::POST, "/api/v4/groups/names".to_owned()),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{NOWHERE}/patch"),
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{NOWHERE}/members"),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{NOWHERE}/members"),
        ),
    ];

    for (method, path) in &routes {
        for (what, body) in &bodies {
            let label = format!("{method} {path} [{what}]");
            let (go_status, go, _) = send(&client, GO, &token, method.clone(), path, body).await;
            let (rs_status, rs, served_by) =
                send(&client, RUST, &token, method.clone(), path, body).await;

            assert_eq!(served_by.as_deref(), Some("rust"), "{label} was forwarded");
            assert_eq!(
                go_status, 501,
                "{label}: Go refuses for the licence, not for the body"
            );
            assert_eq!(rs_status, go_status, "{label}");
            let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
            assert_eq!(parsed["id"], "api.license_error", "{label}");
        }
    }
}

/// **The gate precedes `RequireGroupId`.** An id that is well-formed for the mux but could never
/// be a group still gets the licence error rather than a 400 or a 404.
#[tokio::test]
async fn the_licence_gate_precedes_require_group_id() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // `abc` passes `[A-Za-z0-9]+` and so is routed, but fails `IsValidId` in the handler — which
    // never runs, because the gate is above it.
    for id in ["abc", "0", NOWHERE] {
        for (method, path, body) in every_group_write(id) {
            if !path.contains(id) {
                continue; // the two collection routes carry no id
            }
            let label = format!("{method} {path}");
            let (go_status, go, _) = send(&client, GO, &token, method.clone(), &path, body).await;
            let (rs_status, rs, served_by) =
                send(&client, RUST, &token, method.clone(), &path, body).await;

            assert_eq!(served_by.as_deref(), Some("rust"), "{label} was forwarded");
            assert_eq!(go_status, 501, "{label}");
            assert_eq!(rs_status, go_status, "{label}");
            let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
            assert_ne!(
                parsed["id"], "api.context.invalid_url_param.app_error",
                "{label}: not the id error"
            );
        }
    }
}

/// **A `group_id` outside `[A-Za-z0-9]+` never matched gorilla**, so it is Go's own mux 404 —
/// forwarded, not answered with the licence error. Two paths that look alike, two statuses.
#[tokio::test]
async fn a_group_id_outside_the_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // A hyphen, an underscore and a dot are all outside the class — and all three are legal in a
    // group *name*, which is what makes this the mistake worth guarding.
    for id in ["not-an-id", "not_an_id", "not.an.id"] {
        for (method, path, body) in every_group_write(id) {
            if !path.contains(id) {
                continue;
            }
            let label = format!("{method} {path}");
            let (go_status, go, _) = send(&client, GO, &token, method.clone(), &path, body).await;
            let (rs_status, rs, served_by) =
                send(&client, RUST, &token, method.clone(), &path, body).await;

            assert_eq!(
                served_by.as_deref(),
                Some("go"),
                "{label} must be forwarded so Go answers its own 404"
            );
            assert_eq!(go_status, 404, "{label}: gorilla's NotFoundHandler");
            assert_eq!(rs_status, go_status, "{label}");
            // Compared **raw**, unlike the licence errors: gorilla's `NotFoundHandler` answers
            // before the middleware that mints `X-Request-Id`, so this body has no per-request
            // field and two separate requests produce identical bytes.
            assert_eq!(
                String::from_utf8_lossy(&go),
                String::from_utf8_lossy(&rs),
                "{label}: a forwarded body is Go's own"
            );
        }
    }
}

/// `/names` is a literal for `POST` and an ordinary `{group_id}` for everything else.
///
/// `POST` is ours and is the licence error from `getGroupsByNames`. `DELETE` reaches `deleteGroup`
/// with `group_id = "names"` in Go; this server forwards it, and the two answers must still agree
/// — which they do, because both handlers open with the same gate.
#[tokio::test]
async fn groups_names_is_a_literal_for_the_post_only() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = "/api/v4/groups/names";

    let (go_status, go, _) = send(&client, GO, &token, reqwest::Method::POST, p, "[]").await;
    let (rs_status, rs, served_by) =
        send(&client, RUST, &token, reqwest::Method::POST, p, "[]").await;
    assert_eq!(served_by.as_deref(), Some("rust"), "POST {p} is ours");
    assert_eq!(go_status, 501, "POST {p}");
    assert_eq!(rs_status, go_status, "POST {p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, p);

    // `GET` and `DELETE` are the two other methods gorilla routes at this path — to `getGroup`
    // and `deleteGroup`, both with `group_id = "names"` — and both are re-claimed here, because
    // axum's static-over-parameter preference would otherwise have handed them to the fallback
    // without anything saying so.
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let label = format!("{method} {p}");
        let (go_status, go, _) = send(&client, GO, &token, method.clone(), p, "").await;
        let (rs_status, rs, served_by) = send(&client, RUST, &token, method.clone(), p, "").await;
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{label}: re-claimed from the literal's fallback"
        );
        assert_eq!(go_status, 501, "{label}");
        assert_eq!(rs_status, go_status, "{label}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
        assert_eq!(parsed["id"], "api.license_error", "{label}");
    }

    // Everything gorilla routes nowhere stays forwarded, so Go answers its own mux 404.
    for method in [reqwest::Method::PUT, reqwest::Method::PATCH] {
        let label = format!("{method} {p}");
        let (go_status, go, _) = send(&client, GO, &token, method.clone(), p, "").await;
        let (rs_status, rs, served_by) = send(&client, RUST, &token, method.clone(), p, "").await;
        assert_eq!(rs_status, go_status, "{label}");
        assert_forwarded_bodies_match(&go, &rs, &label);
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{label}: gorilla routes it nowhere, so Go answers its own 404"
        );
    }
}

/// The boundary: a licence row hands every one of the seven back to the proxy, and removing it
/// takes them back.
#[tokio::test]
async fn a_licence_row_hands_every_group_write_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let routes = every_group_write(NOWHERE);

    set_active_licence_id(Some("mmrslicence000000000000002")).await;
    let mut forwarded = Vec::new();
    for (method, path, body) in &routes {
        let (_, _, served_by) = send(&client, RUST, &token, method.clone(), path, body).await;
        forwarded.push((format!("{method} {path}"), served_by));
    }
    // Restored before any assertion, so a failure cannot leave the row set for another suite.
    set_active_licence_id(None).await;

    for (label, answer) in &forwarded {
        assert_eq!(answer.as_deref(), Some("go"), "{label} must be forwarded");
    }

    for (method, path, body) in &routes {
        let (_, _, served_by) = send(&client, RUST, &token, method.clone(), path, body).await;
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{method} {path} after the row is cleared"
        );
    }
}

/// Methods gorilla registers on **none** of these paths must still reach Go rather than axum's
/// 405 — registering three methods on `/groups/{id}/members` must not claim the fourth.
#[tokio::test]
async fn unregistered_methods_still_forward() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (reqwest::Method::PUT, format!("/api/v4/groups/{NOWHERE}")),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{NOWHERE}/members"),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{NOWHERE}/patch"),
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/groups/{NOWHERE}/restore"),
        ),
    ] {
        let label = format!("{method} {path}");
        let (_, _, served_by) = send(&client, RUST, &token, method.clone(), &path, "").await;
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{label} must be forwarded, not answered with our 405"
        );
    }
}
