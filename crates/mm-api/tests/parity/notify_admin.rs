//! Cross-server parity for `POST /api/v4/users/notify-admin` — a user asking the admins to
//! upgrade for a feature.
//!
//! ```sh
//! scripts/parity.sh --test parity notify_admin
//! ```
//!
//! One `NotifyAdmin` row per user and feature. The first request writes it and answers
//! `{"status":"OK"}`; a second for the same feature — any plan — is the 403 `already_notified`.
//! The validator's refusals (a plan that is not `professional`/`enterprise`, a feature outside the
//! paid list) reach the wire as the **500** `app.notify_admin.save.app_error`, because the app
//! layer folds every save error but a not-found into it; a `mattermost.feature.plugin…` feature
//! skips the validator altogether. `null` and a non-object body are the 400 `notifyAdminRequest`.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fixture_pool, go_minted_token, stack_enabled,
};

async fn notify(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/users/notify-admin"))
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

/// `(requiredplan, requiredfeature, trial, sentat)` of the user's rows, feature-ordered.
async fn rows_for(user_id: &str) -> Vec<(String, String, bool, Option<i64>)> {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as(
        "SELECT requiredplan, requiredfeature, trial, sentat FROM notifyadmin WHERE userid = $1 ORDER BY requiredfeature",
    )
    .bind(user_id)
    .fetch_all(&pool)
    .await
    .expect("the rows")
}

/// Each server gets a fresh user: the first request writes the row, the second — same feature,
/// other plan — is the 403, and a different feature is a second row.
#[tokio::test]
async fn the_first_request_writes_a_row_and_the_second_for_the_feature_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "ntfa").await;

    let mut refusals = Vec::new();
    for (base, tag) in [(GO, "ntfag"), (RUST, "ntfar")] {
        let user = create_plain_user(&client, &token, &team, tag).await;
        let (status, served, body) = notify(
            &client,
            base,
            &user.token,
            r#"{"required_feature":"mattermost.feature.guest_accounts","required_plan":"professional","trial_notification":true}"#,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(body, br#"{"status":"OK"}"#, "{base}");
        assert_eq!(
            rows_for(&user.id).await,
            vec![(
                "professional".to_owned(),
                "mattermost.feature.guest_accounts".to_owned(),
                true,
                None
            )],
            "{base}: one row, unsent"
        );

        let (status, served, body) = notify(
            &client,
            base,
            &user.token,
            r#"{"required_feature":"mattermost.feature.guest_accounts","required_plan":"enterprise"}"#,
        )
        .await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(
            parsed["id"], "api.cloud.notify_admin_to_upgrade_error.already_notified",
            "{base}"
        );
        refusals.push(body);
        assert_eq!(rows_for(&user.id).await.len(), 1, "{base}: still one row");

        let (status, _, body) = notify(
            &client,
            base,
            &user.token,
            r#"{"required_feature":"mattermost.feature.custom_user_groups","required_plan":"enterprise"}"#,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        let rows = rows_for(&user.id).await;
        assert_eq!(rows.len(), 2, "{base}");
        assert_eq!(rows[0].0, "enterprise", "{base}: the second feature's plan");
        assert!(!rows[0].2, "{base}: trial defaults to false");
    }
    assert_error_bodies_match_except_known_gaps(&refusals[0], &refusals[1], "already notified");
}

/// The validator's refusals are 500s with the generic save id; a plugin feature skips the
/// validator and is written with whatever plan it names.
#[tokio::test]
async fn an_invalid_plan_or_feature_is_the_generic_500_and_a_plugin_feature_is_not_validated() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "ntfb").await;

    for (label, body) in [
        (
            "bad plan",
            r#"{"required_feature":"mattermost.feature.guest_accounts","required_plan":"gold"}"#,
        ),
        (
            "bad feature",
            r#"{"required_feature":"mattermost.feature.teleport","required_plan":"professional"}"#,
        ),
    ] {
        let mut bodies = Vec::new();
        for (base, tag) in [(GO, "ntfbg"), (RUST, "ntfbr")] {
            // A user per server per case: the tag carries the case, since a username repeats
            // nowhere.
            let user =
                create_plain_user(&client, &token, &team, &format!("{tag}{}", label.len())).await;
            let (status, served, answer) = notify(&client, base, &user.token, body).await;
            assert_eq!(
                status,
                500,
                "{base} {label}: {}",
                String::from_utf8_lossy(&answer)
            );
            assert_eq!(served, base == RUST, "{base} {label}");
            let parsed: serde_json::Value = serde_json::from_slice(&answer).unwrap_or_default();
            assert_eq!(
                parsed["id"], "app.notify_admin.save.app_error",
                "{base} {label}"
            );
            assert!(
                rows_for(&user.id).await.is_empty(),
                "{base} {label}: nothing written"
            );
            bodies.push(answer);
        }
        assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], label);
    }

    for (base, tag) in [(GO, "ntfpg"), (RUST, "ntfpr")] {
        let user = create_plain_user(&client, &token, &team, tag).await;
        let (status, served, body) = notify(
            &client,
            base,
            &user.token,
            r#"{"required_feature":"mattermost.feature.plugin.calls","required_plan":"whatever"}"#,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            rows_for(&user.id).await,
            vec![(
                "whatever".to_owned(),
                "mattermost.feature.plugin.calls".to_owned(),
                false,
                None
            )],
            "{base}: a plugin feature is stored as sent, plan unchecked"
        );
    }
}

/// `null`, an array and a malformed document are the 400 `notifyAdminRequest`; an empty object
/// decodes and fails the validator as a 500.
#[tokio::test]
async fn the_body_is_decoded_like_go_decodes_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "ntfc").await;
    let user = create_plain_user(&client, &token, &team, "ntfc").await;

    for (body, status, id) in [
        ("null", 400, "api.context.invalid_body_param.app_error"),
        ("[]", 400, "api.context.invalid_body_param.app_error"),
        ("{", 400, "api.context.invalid_body_param.app_error"),
        ("{}", 500, "app.notify_admin.save.app_error"),
    ] {
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let (got, served, answer) = notify(&client, base, &user.token, body).await;
            assert_eq!(
                got,
                status,
                "{base} {body}: {}",
                String::from_utf8_lossy(&answer)
            );
            assert_eq!(served, base == RUST, "{base} {body}");
            let parsed: serde_json::Value = serde_json::from_slice(&answer).unwrap_or_default();
            assert_eq!(parsed["id"], id, "{base} {body}");
            bodies.push(answer);
        }
        assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], body);
    }
    assert!(rows_for(&user.id).await.is_empty());
}
