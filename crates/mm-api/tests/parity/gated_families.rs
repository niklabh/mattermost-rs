//! Cross-server parity for the last fourteen routes: `compliance.go`, `ip_filtering.go`,
//! `ai_bridge_test_helper.go` and `scheduled_post.go`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity gated_families
//! ```
//!
//! # Four families, four refusals, two statuses
//!
//! Compliance and IP filtering answer 501; the AI-bridge helper answers 501 with a *config*
//! reason; scheduled posts answer **400**, from a gate with two arms that share that status and
//! differ only in id. Every one of those is a shape a reader could borrow from a neighbour, so
//! each is asserted with its own id rather than through a shared helper.

use crate::common;

use common::{ACTIVE_LICENCE_ROW, GO, RUST, client, go_minted_token, stack_enabled};

const COMPLIANCE_ERROR: &str = "ent.compliance.licence_disable.app_error";
const IP_FILTERING_ERROR: &str = "api.context.ip_filtering.not_available.app_error";
const AI_BRIDGE_ERROR: &str = "api.ai_bridge_test_helper.disabled.app_error";
const SCHEDULED_POSTS_ERROR: &str = "api.scheduled_posts.license_error";
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

fn m(s: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(s.as_bytes()).expect("a method")
}

/// `(method, path, expected status, expected id)` for all fourteen.
fn routes() -> Vec<(reqwest::Method, String, u16, &'static str)> {
    vec![
        (
            m("GET"),
            "/api/v4/compliance/reports".into(),
            501,
            COMPLIANCE_ERROR,
        ),
        (
            m("POST"),
            "/api/v4/compliance/reports".into(),
            501,
            COMPLIANCE_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/compliance/reports/{NOWHERE}"),
            501,
            COMPLIANCE_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/compliance/reports/{NOWHERE}/download"),
            501,
            COMPLIANCE_ERROR,
        ),
        (
            m("GET"),
            "/api/v4/ip_filtering".into(),
            501,
            IP_FILTERING_ERROR,
        ),
        (
            m("POST"),
            "/api/v4/ip_filtering".into(),
            501,
            IP_FILTERING_ERROR,
        ),
        (
            m("GET"),
            "/api/v4/ip_filtering/my_ip".into(),
            501,
            IP_FILTERING_ERROR,
        ),
        (
            m("GET"),
            "/api/v4/system/e2e/ai_bridge".into(),
            501,
            AI_BRIDGE_ERROR,
        ),
        (
            m("PUT"),
            "/api/v4/system/e2e/ai_bridge".into(),
            501,
            AI_BRIDGE_ERROR,
        ),
        (
            m("DELETE"),
            "/api/v4/system/e2e/ai_bridge".into(),
            501,
            AI_BRIDGE_ERROR,
        ),
        (
            m("POST"),
            "/api/v4/posts/schedule".into(),
            400,
            SCHEDULED_POSTS_ERROR,
        ),
        (
            m("PUT"),
            format!("/api/v4/posts/schedule/{NOWHERE}"),
            400,
            SCHEDULED_POSTS_ERROR,
        ),
        (
            m("DELETE"),
            format!("/api/v4/posts/schedule/{NOWHERE}"),
            400,
            SCHEDULED_POSTS_ERROR,
        ),
        (
            m("GET"),
            format!("/api/v4/posts/scheduled/team/{NOWHERE}"),
            400,
            SCHEDULED_POSTS_ERROR,
        ),
    ]
}

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

/// All fourteen, each with **its own** status and id.
#[tokio::test]
async fn every_route_gives_its_own_familys_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let all = routes();
    assert_eq!(all.len(), 14);

    for (method, path, want_status, want_id) in &all {
        let (status, parsed) = both(&client, &token, method, path, b"{}").await;
        assert_eq!(status, *want_status, "{method} {path}");
        assert_eq!(parsed["id"], *want_id, "{method} {path}");
    }

    // Four distinct ids and two distinct statuses, so the loop is not asserting one constant.
    let ids: std::collections::BTreeSet<_> = all.iter().map(|r| r.3).collect();
    assert_eq!(ids.len(), 4, "four families, four refusals");
    let statuses: std::collections::BTreeSet<_> = all.iter().map(|r| r.2).collect();
    assert_eq!(statuses, [400, 501].into_iter().collect());
}

/// **Compliance checks its permission and its id first**, unlike the three families beside it.
///
/// Three permissions across four routes, and the download's is not the read one — so a caller
/// holding only `read_compliance_export_job` gets past two routes and is refused the download.
#[tokio::test]
async fn compliance_checks_come_before_the_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    common::purge_api_fixtures().await;
    let team = common::create_team(&client, &admin, "compl").await;

    let reader = async |tag: &str, permission: &str| -> Option<String> {
        let role = common::plant_role(tag, permission).await?;
        let user = common::create_plain_user(&client, &admin, &team, tag).await;
        common::set_user_roles(&user.id, &format!("system_user {role}")).await;
        Some(common::login_plain_user(&client, tag).await)
    };
    let Some(read_only) = reader("complread", "read_compliance_export_job").await else {
        return; // no DATABASE_URL
    };

    // The read permission opens the list and the single report...
    for path in [
        "/api/v4/compliance/reports".to_owned(),
        format!("/api/v4/compliance/reports/{NOWHERE}"),
    ] {
        let (status, _) = both(&client, &read_only, &m("GET"), &path, b"").await;
        assert_eq!(status, 501, "the read permission admits: {path}");
    }

    // ...and **not** the download, which wants a third permission on the same resource.
    let download = format!("/api/v4/compliance/reports/{NOWHERE}/download");
    let (status, parsed) = both(&client, &read_only, &m("GET"), &download, b"").await;
    assert_eq!(
        status, 403,
        "`download_compliance_export_result` is not `read_compliance_export_job`"
    );
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    // Nor the create, which wants a second.
    let (status, _) = both(
        &client,
        &read_only,
        &m("POST"),
        "/api/v4/compliance/reports",
        b"{}",
    )
    .await;
    assert_eq!(status, 403, "creating wants `create_compliance_export_job`");

    // **The id check is alive here**, unlike `/data_retention`'s: a malformed report id is a 400
    // and not the licence refusal.
    for path in [
        "/api/v4/compliance/reports/short".to_owned(),
        "/api/v4/compliance/reports/short/download".to_owned(),
    ] {
        let (status, parsed) = both(&client, &admin, &m("GET"), &path, b"").await;
        assert_eq!(status, 400, "`RequireReportId`'s result is checked: {path}");
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    }

    // And a malformed body is a 400 on the create, because the decode comes first.
    let (status, parsed) = both(
        &client,
        &admin,
        &m("POST"),
        "/api/v4/compliance/reports",
        b"{",
    )
    .await;
    assert_eq!(status, 400, "the body is decoded before the permission");
    assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
}

/// **The other three families check nothing** — a plain user gets the same refusal an admin does.
#[tokio::test]
async fn the_gate_first_families_check_nothing() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "gated").await;
    let plain = common::create_plain_user(&client, &admin, &team, "gated").await;

    for (method, path, want_status, want_id) in routes() {
        if path.starts_with("/api/v4/compliance") {
            continue; // that family does check — see the test above
        }
        let (status, parsed) = both(&client, &plain.token, &method, &path, b"{}").await;
        assert_eq!(
            status, want_status,
            "no permission is checked ahead of the gate: {method} {path}"
        );
        assert_eq!(parsed["id"], want_id);

        // A malformed body is not decoded either.
        if method != reqwest::Method::GET {
            let (status, _) = both(&client, &plain.token, &method, &path, b"{").await;
            assert_eq!(
                status, want_status,
                "the body is never read: {method} {path}"
            );
        }
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **`EnableTesting` turns the AI-bridge routes over to Go**, and it lives only in the
/// environment — so this uses a second server, as `/recaps` does.
#[tokio::test]
async fn enabling_testing_forwards_the_ai_bridge_routes() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let Some(server) =
        common::SecondServer::start(8078, &[("MM_SERVICESETTINGS_ENABLETESTING", "true")]).await
    else {
        return;
    };
    let client = client();
    let token = go_minted_token(&client).await;

    for method in [m("GET"), m("PUT"), m("DELETE")] {
        let response = client
            .request(
                method.clone(),
                format!("{}/api/v4/system/e2e/ai_bridge", server.base),
            )
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(b"{}".to_vec())
            .send()
            .await
            .expect("the second server answers");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "with testing enabled, {method} /system/e2e/ai_bridge must be forwarded"
        );
    }

    // The ordinary server still answers, so the difference is the configuration.
    let (status, parsed) = both(
        &client,
        &token,
        &m("GET"),
        "/api/v4/system/e2e/ai_bridge",
        b"",
    )
    .await;
    assert_eq!(status, 501);
    assert_eq!(parsed["id"], AI_BRIDGE_ERROR);
}

/// **Turning scheduled posts off changes the id, not the status** — the gate's other arm.
#[tokio::test]
async fn disabling_scheduled_posts_changes_the_error_id() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let Some(server) =
        common::SecondServer::start(8079, &[("MM_SERVICESETTINGS_SCHEDULEDPOSTS", "false")]).await
    else {
        return;
    };
    let client = client();
    let token = go_minted_token(&client).await;

    let response = client
        .post(format!("{}/api/v4/posts/schedule", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("the second server answers");
    assert_eq!(
        response.status(),
        400,
        "both arms of the gate are a 400 — only the id moves"
    );
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(
        body["id"], "api.scheduled_posts.feature_disabled",
        "the config arm names the feature; the licence arm names the licence"
    );

    // And the ordinary server, with the setting at its default `true`, reaches the licence arm.
    let (status, parsed) = both(&client, &token, &m("POST"), "/api/v4/posts/schedule", b"{}").await;
    assert_eq!(status, 400);
    assert_eq!(parsed["id"], SCHEDULED_POSTS_ERROR);
}

/// A licence hands the licence-gated ones back to Go — and **not** the AI-bridge routes, whose
/// gate is a setting.
#[tokio::test]
async fn a_licence_forwards_only_the_licence_gated_families() {
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

    common::set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut observed = Vec::new();
    for (method, path, _, id) in routes() {
        observed.push((
            format!("{method} {path}"),
            id,
            served_by(&method, &path).await,
        ));
    }
    common::set_active_licence_id(None).await;

    for (route, id, served) in &observed {
        if *id == AI_BRIDGE_ERROR {
            assert_eq!(
                served.as_deref(),
                Some("rust"),
                "{route} is gated on a setting, not a licence, so a licence changes nothing"
            );
        } else {
            assert_eq!(
                served.as_deref(),
                Some("go"),
                "{route} is licence-gated and must be forwarded"
            );
        }
    }
}

/// Registering fourteen methods must not turn their neighbours into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (m("DELETE"), "/api/v4/compliance/reports".to_owned()),
        (m("PUT"), format!("/api/v4/compliance/reports/{NOWHERE}")),
        (m("DELETE"), "/api/v4/ip_filtering".to_owned()),
        (m("POST"), "/api/v4/system/e2e/ai_bridge".to_owned()),
        (m("GET"), "/api/v4/posts/schedule".to_owned()),
        (m("PATCH"), format!("/api/v4/posts/schedule/{NOWHERE}")),
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
