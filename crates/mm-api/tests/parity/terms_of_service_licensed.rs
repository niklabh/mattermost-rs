//! `POST /api/v4/terms_of_service` on the **licensed** pair — `createTermsOfService` past
//! `license.Features.CustomTermsOfService`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity terms_of_service_licensed
//! ```
//!
//! `parity::terms_of_service_writes` measures the 400 on the stack's unlicensed pair; this
//! measures the publish against the Enterprise-licensed oracle, whose licence has every feature
//! on. Two facts [D-382] could only transcribe are measured here: a publish is a **200** (not a
//! 201) carrying the new row, and re-posting identical text returns the *existing* row — id and
//! `create_at` included — so a revision published through Go and re-posted through us comes back
//! byte for byte.
//!
//! # The table is shared, and Go caches "the latest"
//!
//! Every row this suite publishes becomes the latest revision for the unlicensed pair's
//! `GET /terms_of_service` too, and both Go servers cache that read. So the suite holds
//! [`common::GO_CACHE`], deletes what it wrote, and invalidates both Go servers' caches before
//! releasing it — the same hazard the mutation memory records, closed the same way.

use crate::common;

use common::{
    GO_CACHE, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, delete_plain_user, go_minted_token, invalidate_go_caches_locked,
    invalidate_licensed_go_caches, licensed, request_raw, stack_enabled,
};

const PATH: &str = "/api/v4/terms_of_service";

async fn publish(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    text: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let body = serde_json::json!({ "text": text }).to_string();
    request_raw(
        client,
        base,
        reqwest::Method::POST,
        Some(token),
        PATH,
        Some(body.as_bytes()),
    )
    .await
}

/// Delete every revision this suite published, so the unlicensed pair's "latest" is what it was.
async fn purge(prefix: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    sqlx::query("DELETE FROM termsofservice WHERE text LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(&pool)
        .await
        .expect("purges the suite's revisions");
}

/// A publish through Go, the same text through us — the existing row, byte for byte — and a new
/// text through us, which Go then returns unchanged.
#[tokio::test]
async fn identical_text_returns_the_existing_row_and_new_text_publishes() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let _cache = GO_CACHE.lock().await;
    let prefix = format!("mmrs licensed tos {}", mm_model::utils::new_id());

    let text_a = format!("{prefix} a");
    let (go_status, go_body, _) = publish(&client, &pair.go, &admin, &text_a).await;
    assert_eq!(
        go_status,
        200,
        "a publish is a 200, not a 201: {}",
        String::from_utf8_lossy(&go_body)
    );
    let (rs_status, rs_body, served) = publish(&client, &pair.rust, &admin, &text_a).await;
    assert_eq!(served.as_deref(), Some("rust"), "served, not forwarded");
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "identical text returns Go's row, id and create_at included"
    );
    assert!(
        rs_body.ends_with(b"\n"),
        "json.NewEncoder: a trailing newline"
    );
    let row: serde_json::Value = serde_json::from_slice(&go_body).unwrap();
    assert_eq!(row["text"], text_a);
    assert_eq!(row["user_id"], common::logged_in_user_id());

    let text_b = format!("{prefix} b");
    let (rs_status, rs_body, served) = publish(&client, &pair.rust, &admin, &text_b).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let ours: serde_json::Value = serde_json::from_slice(&rs_body).unwrap();
    assert_ne!(ours["id"], row["id"], "different text is a new revision");
    assert_eq!(ours["text"], text_b);
    // Go's cache still holds A as the latest; the oracle has to be told before it can see B.
    invalidate_licensed_go_caches(&client, &pair, &admin).await;
    let (go_status, go_body, _) = publish(&client, &pair.go, &admin, &text_b).await;
    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "Go returns the row we published, byte for byte"
    );

    purge(&prefix).await;
    invalidate_go_caches_locked(&client, &admin).await;
    invalidate_licensed_go_caches(&client, &pair, &admin).await;
}

/// The two refusals past the licence: empty text (400, under `Config.IsValid`) and a non-admin
/// (403) — both the same on the licensed pair, so the licence gate is not what refused them.
#[tokio::test]
async fn empty_text_and_a_non_admin_are_refused_past_the_licence() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "ltos").await;

    let (go_status, go_body, _) = publish(&client, &pair.go, &admin, "").await;
    let (rs_status, rs_body, served) = publish(&client, &pair.rust, &admin, "").await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 400, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "empty text");
    assert_eq!(
        parsed["id"],
        "api.create_terms_of_service.empty_text.app_error"
    );

    let (go_status, go_body, _) = publish(&client, &pair.go, &plain.token, "mmrs never").await;
    let (rs_status, rs_body, served) =
        publish(&client, &pair.rust, &plain.token, "mmrs never").await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "non-admin");
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    delete_plain_user(&client, &admin, &plain.id).await;
}
