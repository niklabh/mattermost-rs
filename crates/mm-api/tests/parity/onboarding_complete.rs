//! Cross-server parity for `POST /api/v4/system/onboarding/complete`.
//!
//! ```sh
//! scripts/parity.sh --test parity onboarding_complete
//! ```
//!
//! Both servers write the same two `Systems` rows, so the write is made on each and read back
//! from the table; a request naming plugins is handed to Go.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fixture_pool, go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: &str,
) -> (u16, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/system/onboarding/complete"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
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

async fn system_value(pool: &sqlx::PgPool, name: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM systems WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .expect("the row reads")
}

/// The refusals in order, the two rows written by each server, and the plugin hand-over.
#[tokio::test]
async fn the_rows_are_written_and_the_plugin_install_is_gos() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "onb").await;
    let user = create_plain_user(&client, &admin, &team, "onb").await;

    for (token, body, status, id) in [
        (
            &user.token,
            r#"{"organization":"Acme"}"#,
            403,
            "app.system.complete_onboarding_request.no_first_user",
        ),
        (
            &admin,
            "[]",
            400,
            "app.system.complete_onboarding_request.app_error",
        ),
        (
            &admin,
            "",
            400,
            "app.system.complete_onboarding_request.app_error",
        ),
        (
            &admin,
            r#"{"organization":""}"#,
            400,
            "api.error_no_organization_name_provided_for_self_hosted_onboarding",
        ),
        (
            &admin,
            "{}",
            400,
            "api.error_no_organization_name_provided_for_self_hosted_onboarding",
        ),
    ] {
        let (go_status, _, go) = post(&client, GO, token, body).await;
        let (rs_status, served, rs) = post(&client, RUST, token, body).await;
        assert_eq!(
            go_status,
            status,
            "Go {body}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            status,
            "Rust {body}: {}",
            String::from_utf8_lossy(&rs)
        );
        assert_eq!(served.as_deref(), Some("rust"), "{body}: served here");
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(parsed["id"], id, "{body}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/system/onboarding/complete");
    }

    for (base, org) in [(GO, "Acme by Go"), (RUST, "Acme by Rust")] {
        let (status, served, body) = post(
            &client,
            base,
            &admin,
            &format!(r#"{{"organization":"{org}","install_plugins":[]}}"#),
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        // A request straight to Go carries no served-by header; ours says `rust`.
        assert_eq!(
            served.as_deref(),
            if base == RUST { Some("rust") } else { None }
        );
        assert_eq!(
            body, br#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no newline"
        );
        assert_eq!(
            system_value(&pool, "OrganizationName").await.as_deref(),
            Some(org),
            "{base}"
        );
        assert_eq!(
            system_value(&pool, "FirstAdminSetupComplete")
                .await
                .as_deref(),
            Some("true"),
            "{base}"
        );
    }

    let (_, served, _) = post(
        &client,
        RUST,
        &admin,
        r#"{"organization":"Acme","install_plugins":["com.mattermost.nps"]}"#,
    )
    .await;
    assert_eq!(served.as_deref(), Some("go"), "the plugin install is Go's");
}
