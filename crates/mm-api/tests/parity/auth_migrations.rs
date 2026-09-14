//! Cross-server parity for the two authentication migrations and the cloud login —
//! `POST /api/v4/users/migrate_auth/ldap`, `POST /api/v4/users/migrate_auth/saml` and
//! `POST /api/v4/users/login/cws`.
//!
//! ```sh
//! scripts/parity.sh --test parity auth_migrations
//! ```
//!
//! On this build the migrations end in the 501 `not_available` for every caller who clears the
//! body and the permission — the licence arm and the nil `AccountMigration()` arm carry the same
//! id, so the licensed pair agrees — and the cloud login is the 401 `login_cws.license.error`
//! before it reads anything, since no licence here is a Cloud one.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let mut request = client
        .post(format!("{base}{path}"))
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

fn id_of(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Both servers at `base_go`/`base_rust`; the status, our serving, the id, and the bodies.
async fn both(
    client: &reqwest::Client,
    bases: (&str, &str),
    token: Option<&str>,
    path: &str,
    body: &str,
    status: u16,
    id: &str,
) {
    let (go_status, _, go) = post(client, bases.0, token, path, body).await;
    let (rs_status, served, rs) = post(client, bases.1, token, path, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {path} {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {path} {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{path} {body}: served here");
    assert_eq!(id_of(&go), id, "{path} {body}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, path);
}

/// The LDAP migration: each body fault its own 400 in Go's order, the plain user's 403, and the
/// admin's 501 — on the unlicensed pair and, with the same id, on the licensed one.
#[tokio::test]
async fn the_ldap_migration_validates_then_refuses_with_the_same_501_on_both_pairs() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "amig").await;
    let user = create_plain_user(&client, &token, &team, "amig").await;
    let path = "/api/v4/users/migrate_auth/ldap";
    let unlicensed = (GO, RUST);

    for (body, param) in [
        ("[]", "from"),
        (r#"{"force":true,"match_field":"email"}"#, "from"),
        (r#"{"from":"","force":true,"match_field":"email"}"#, "from"),
        (
            r#"{"from":"ldap","force":true,"match_field":"email"}"#,
            "from",
        ),
        (r#"{"from":"email","match_field":"email"}"#, "force"),
        (
            r#"{"from":"email","force":"yes","match_field":"email"}"#,
            "force",
        ),
        (r#"{"from":"email","force":true}"#, "match_field"),
        (
            r#"{"from":"email","force":true,"match_field":1}"#,
            "match_field",
        ),
    ] {
        both(
            &client,
            unlicensed,
            Some(&user.token),
            path,
            body,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
        let (_, _, go) = post(&client, GO, Some(&user.token), path, body).await;
        let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap_or_default();
        assert!(
            parsed["message"]
                .as_str()
                .is_some_and(|m| m.contains(param)),
            "{body}: the 400 names {param}: {parsed}"
        );
    }

    let valid = r#"{"from":"saml","force":false,"match_field":"username"}"#;
    both(
        &client,
        unlicensed,
        Some(&user.token),
        path,
        valid,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both(
        &client,
        unlicensed,
        Some(&token),
        path,
        valid,
        501,
        "api.admin.ldap.not_available.app_error",
    )
    .await;

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    both(
        &client,
        (&pair.go, &pair.rust),
        Some(&token),
        path,
        valid,
        501,
        "api.admin.ldap.not_available.app_error",
    )
    .await;
}

/// The SAML migration, the same way: `auto` and an object `matches` after `from`.
#[tokio::test]
async fn the_saml_migration_validates_then_refuses_with_the_same_501_on_both_pairs() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "smig").await;
    let user = create_plain_user(&client, &token, &team, "smig").await;
    let path = "/api/v4/users/migrate_auth/saml";
    let unlicensed = (GO, RUST);

    for (body, param) in [
        ("null", "from"),
        (r#"{"from":"saml","auto":true,"matches":{}}"#, "from"),
        (r#"{"from":"ldap","matches":{}}"#, "auto"),
        (r#"{"from":"ldap","auto":true}"#, "matches"),
        (r#"{"from":"ldap","auto":true,"matches":[]}"#, "matches"),
    ] {
        both(
            &client,
            unlicensed,
            Some(&user.token),
            path,
            body,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
        let (_, _, go) = post(&client, GO, Some(&user.token), path, body).await;
        let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap_or_default();
        assert!(
            parsed["message"]
                .as_str()
                .is_some_and(|m| m.contains(param)),
            "{body}: the 400 names {param}: {parsed}"
        );
    }

    let valid = r#"{"from":"ldap","auto":false,"matches":{"a@example.com":"a@saml.example"}}"#;
    both(
        &client,
        unlicensed,
        Some(&user.token),
        path,
        valid,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both(
        &client,
        unlicensed,
        Some(&token),
        path,
        valid,
        501,
        "api.admin.saml.not_available.app_error",
    )
    .await;

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    both(
        &client,
        (&pair.go, &pair.rust),
        Some(&token),
        path,
        valid,
        501,
        "api.admin.saml.not_available.app_error",
    )
    .await;
}

/// The cloud login: no session needed, and the 401 `login_cws.license.error` before the form
/// is read — with no licence, and with the Enterprise one, which is not Cloud.
#[tokio::test]
async fn the_cloud_login_is_the_401_on_both_pairs() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/login/cws";

    for session in [None, Some(token.as_str())] {
        both(
            &client,
            (GO, RUST),
            session,
            path,
            "login_id=nobody&token=nothing",
            401,
            "api.user.login_cws.license.error",
        )
        .await;
    }

    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    both(
        &client,
        (&pair.go, &pair.rust),
        None,
        path,
        "",
        401,
        "api.user.login_cws.license.error",
    )
    .await;
}
