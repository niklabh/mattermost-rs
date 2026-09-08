//! Cross-server parity for the ten reads in `mm_api::gated_reads`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity gated_reads
//! ```
//!
//! # What is actually under test
//!
//! Not "does it refuse" — every one of them refuses. The three things a port gets wrong here are
//! **which status**, **what runs before the refusal**, and **whether a session is required at
//! all**; each has its own test below rather than being folded into a loop over the ten.
//!
//! Two of the ten answer **403** where the family's convention is 501, and one of those two —
//! `api.ldap_groups.license_error` — shares its id with a `group.go` route that answers 501. So
//! neither the id nor the status can be inferred from the other.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, fetch_both_raw, go_minted_token, set_active_licence_id,
    stack_enabled,
};

const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// `(path, status, id)` for all ten, as an admin sees them.
fn every_gated_read() -> Vec<(String, u16, &'static str)> {
    vec![
        (
            "/api/v4/hosted_customer/signup_available".into(),
            501,
            "api.server.hosted_signup_unavailable.error",
        ),
        (
            "/api/v4/trial-license/prev".into(),
            403,
            "api.license.upgrade_needed.app_error",
        ),
        (
            "/api/v4/saml/metadata".into(),
            501,
            "api.admin.saml.not_available.app_error",
        ),
        (
            "/api/v4/ldap/groups".into(),
            501,
            "api.ldap_groups.license_error",
        ),
        (
            "/api/v4/system/support_packet".into(),
            403,
            "api.no_license",
        ),
        (
            "/api/v4/custom_profile_attributes/group".into(),
            403,
            "app.property.license_error",
        ),
        (
            "/api/v4/users/sessions/attributes/manifest".into(),
            501,
            "api.user.session_attributes.disabled.app_error",
        ),
        (
            "/api/v4/oauth/outgoing_connections".into(),
            501,
            "api.context.outgoing_oauth_connection.not_available.configuration_disabled",
        ),
        (
            format!("/api/v4/oauth/outgoing_connections/{NOWHERE}"),
            501,
            "api.context.outgoing_oauth_connection.not_available.configuration_disabled",
        ),
        (
            format!("/api/v4/jobs/{NOWHERE}/download"),
            501,
            "app.job.download_export_results_not_enabled",
        ),
    ]
}

async fn served_by(client: &reqwest::Client, token: &str, path: &str) -> Option<String> {
    client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer")
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// All ten, each with **its own** status and id, and each served by us rather than forwarded.
#[tokio::test]
async fn every_gated_read_gives_its_own_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = every_gated_read();
    assert_eq!(all.len(), 10);

    for (path, want_status, want_id) in &all {
        let ((go_status, go), (rs_status, rs)) =
            fetch_both_raw(&client, token.as_str(), path).await;
        assert_eq!(
            go_status,
            *want_status,
            "{path}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(body["id"], *want_id, "{path}");
        assert!(!rs.ends_with(b"\n"), "{path}: an error body has no newline");
        assert_eq!(
            served_by(&client, &token, path).await.as_deref(),
            Some("rust"),
            "{path} was forwarded"
        );
    }

    // Nine distinct ids across ten routes, and exactly two statuses — the guard against a loop
    // that asserts one constant ten times. **Nine, not ten**: the two outgoing-OAuth routes share
    // `configuration_disabled` because they share `ensureOutgoingOAuthConnectionInterface`, and
    // every other route in the family has an id of its own.
    let ids: std::collections::BTreeSet<_> = all.iter().map(|r| r.2).collect();
    assert_eq!(
        ids.len(),
        9,
        "ten routes and nine ids — only the outgoing-OAuth pair shares one"
    );
    let statuses: std::collections::BTreeSet<_> = all.iter().map(|r| r.1).collect();
    assert_eq!(statuses, [403, 501].into_iter().collect());
}

/// **`api.ldap_groups.license_error` at 501 here and at 403 in `group.go`.** One id, two statuses,
/// two files — asserted side by side because that is the only way to see it.
#[tokio::test]
async fn the_shared_ldap_id_carries_a_different_status_in_each_file() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let ldap = "/api/v4/ldap/groups";
    let ((ldap_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, ldap).await;
    assert_eq!(rs_status, ldap_status);
    let ldap_body = assert_error_bodies_match_except_known_gaps(&go, &rs, ldap);

    // `getGroupStats` reaches `requireLicense` first on an unlicensed server, so its own
    // `api.ldap_groups.license_error` at 403 is behind that. The point stands the other way: the
    // ids are shared and nothing about one route's status can be read off the other's.
    let stats = format!("/api/v4/groups/{NOWHERE}/stats");
    let ((stats_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &stats).await;
    assert_eq!(rs_status, stats_status);
    let stats_body = assert_error_bodies_match_except_known_gaps(&go, &rs, &stats);

    assert_eq!(ldap_body["id"], "api.ldap_groups.license_error");
    assert_eq!(ldap_status, 501);
    assert_eq!(stats_body["id"], "api.license_error", "the outer gate wins");
    assert_eq!(stats_status, 501);
}

/// **Two of the ten take no session at all.** `APIHandler`, not `APISessionRequired`: an
/// unauthenticated request reaches the handler and gets the refusal, where the other eight answer
/// 401 first. Adding a session extractor "for consistency" would turn a 501 into a 401.
#[tokio::test]
async fn the_two_apihandler_routes_answer_without_a_session() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();

    let anonymous = async |base: &str, path: &str| {
        let response = client
            .get(format!("{base}{path}"))
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

    for (path, want_status, want_id) in [
        (
            "/api/v4/saml/metadata",
            501,
            "api.admin.saml.not_available.app_error",
        ),
        (
            "/api/v4/users/sessions/attributes/manifest",
            501,
            "api.user.session_attributes.disabled.app_error",
        ),
    ] {
        let (go_status, _, go) = anonymous(GO, path).await;
        let (rs_status, served_by, rs) = anonymous(RUST, path).await;
        assert_eq!(
            go_status,
            want_status,
            "{path} with no token: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(served_by.as_deref(), Some("rust"), "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(body["id"], want_id, "{path}");
    }

    // And the contrast: a session-required neighbour is a 401 for the same anonymous request.
    let (go_status, _, go) = anonymous(GO, "/api/v4/ldap/groups").await;
    let (rs_status, _, rs) = anonymous(RUST, "/api/v4/ldap/groups").await;
    assert_eq!(go_status, 401, "the other eight need a session");
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/ldap/groups");
    assert_eq!(body["id"], "api.context.session_expired.app_error");
}

/// **The permission runs before the gate on three of the ten**, so a plain user gets a 403 naming
/// a permission rather than the family's refusal — and never learns whether the server is
/// licensed or how it is configured.
#[tokio::test]
async fn the_permission_checks_come_before_the_refusals() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "gatedread").await;
    let user = create_plain_user(&client, &admin, &team, "gatedread").await;

    // Three routes whose permission fires first…
    for path in [
        "/api/v4/ldap/groups".to_owned(),
        "/api/v4/system/support_packet".to_owned(),
        "/api/v4/oauth/outgoing_connections".to_owned(),
        format!("/api/v4/oauth/outgoing_connections/{NOWHERE}"),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &path).await;
        assert_eq!(
            go_status,
            403,
            "{path}: the permission is checked before the gate: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "api.context.permissions.app_error", "{path}");
        assert_eq!(
            served_by(&client, &user.token, &path).await.as_deref(),
            Some("rust"),
            "{path}"
        );
    }

    // …and the ones whose gate is first, which give a plain user the *same* answer as an admin.
    for path in [
        "/api/v4/hosted_customer/signup_available",
        "/api/v4/trial-license/prev",
        "/api/v4/custom_profile_attributes/group",
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, path).await;
        assert_eq!(rs_status, go_status, "{path}");
        let plain = assert_error_bodies_match_except_known_gaps(&go, &rs, path);

        let ((admin_status, go), _) = fetch_both_raw(&client, &admin, path).await;
        let as_admin: serde_json::Value = serde_json::from_slice(&go).expect("json");
        assert_eq!(
            go_status, admin_status,
            "{path}: no permission is consulted, so both callers get the same status"
        );
        assert_eq!(plain["id"], as_admin["id"], "{path}");
    }

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// **`downloadJob` validates the id before it reads the setting.** A malformed id is a 400 and a
/// well-formed one that names no job is the 501 — the job is never fetched, so there is no 404 on
/// this route at all.
#[tokio::test]
async fn download_job_checks_the_id_before_the_setting() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let short = "/api/v4/jobs/short/download";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, short).await;
    assert_eq!(go_status, 400, "{short}");
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, short);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // A *real* job id gives the same 501 as a nonexistent one — the setting is checked before the
    // fetch, so the route cannot tell a caller whether the job exists.
    let jobs: serde_json::Value = client
        .get(format!("{RUST}/api/v4/jobs/type/migrations"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable")
        .json()
        .await
        .expect("an array");
    let real = jobs[0]["id"].as_str().expect("the migrations job exists");

    for id in [real, NOWHERE] {
        let path = format!("/api/v4/jobs/{id}/download");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 501, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "app.job.download_export_results_not_enabled");
    }
}

/// The boundary: a licence row hands the licence-gated members of the family back to Go, and the
/// two that are **not** licence questions — the feature flag and the config setting — keep
/// answering, because a licence changes neither.
#[tokio::test]
async fn a_license_row_moves_only_the_licence_gated_ones() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let licence_gated = [
        "/api/v4/trial-license/prev",
        "/api/v4/saml/metadata",
        "/api/v4/ldap/groups",
        "/api/v4/system/support_packet",
        "/api/v4/custom_profile_attributes/group",
    ];
    // Neither of these consults the licence at all: the flag short-circuits before it, and the
    // setting is checked before it.
    let not_licence_gated = [
        "/api/v4/users/sessions/attributes/manifest",
        "/api/v4/oauth/outgoing_connections",
        "/api/v4/hosted_customer/signup_available",
    ];

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut observed = Vec::new();
    for path in licence_gated.iter().chain(not_licence_gated.iter()) {
        observed.push((*path, served_by(&client, &token, path).await));
    }
    // Restored before any assertion, so a failure cannot leave the row set for another suite.
    set_active_licence_id(None).await;

    for (path, answer) in &observed {
        let expected = if licence_gated.contains(path) {
            "go"
        } else {
            "rust"
        };
        assert_eq!(answer.as_deref(), Some(expected), "{path} while licensed");
    }

    for path in licence_gated.iter().chain(not_licence_gated.iter()) {
        assert_eq!(
            served_by(&client, &token, path).await.as_deref(),
            Some("rust"),
            "{path} once the licence is gone"
        );
    }
}

/// Registering these `GET`s must not turn their `POST`/`PUT`/`DELETE` siblings into our 405.
#[tokio::test]
async fn other_methods_on_the_same_paths_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (reqwest::Method::POST, "/api/v4/oauth/outgoing_connections"),
        (
            reqwest::Method::DELETE,
            "/api/v4/oauth/outgoing_connections/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        ),
    ] {
        let ours = client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {path} must still be forwarded"
        );
        assert_ne!(ours.status().as_u16(), 405, "{method} {path}");
    }
}
