//! Cross-server parity for `PUT /api/v4/system/notices/view`.
//!
//! ```sh
//! scripts/parity.sh --test parity product_notices_view
//! ```
//!
//! A write per server on its own user, read back from `ProductNoticeViewState`: one row per
//! notice with `Viewed` counting the requests that named it, `Timestamp` in seconds, and a list
//! naming a notice twice counting once.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fixture_pool, go_minted_token, stack_enabled,
};

async fn put(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let mut request = client
        .put(format!("{base}/api/v4/system/notices/view"))
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

/// Only the rows this suite writes: Go marks every *cached* notice viewed for a new account
/// (`UpdateViewedProductNoticesForNewUser`), and the stack's Go has a cache, so a fresh user
/// already has a row per current notice.
async fn rows(pool: &sqlx::PgPool, user_id: &str) -> Vec<(String, i32, i64)> {
    sqlx::query_as(
        "SELECT noticeid, viewed, \"timestamp\" FROM productnoticeviewstate WHERE userid = $1 AND noticeid LIKE 'notice-%' ORDER BY noticeid",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

/// Every row of the user's, the cached-notice ones included — what the empty forms must leave
/// exactly as it was, since a port that wrote a row for `null` would file it under `""`.
async fn total_rows(pool: &sqlx::PgPool, user_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM productnoticeviewstate WHERE userid = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The write on each server: rows created, counted up, de-duplicated, and the empty forms.
#[tokio::test]
async fn viewing_writes_counts_and_dedupes_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "pnv").await;

    for (base, tag) in [(GO, "pnvgo"), (RUST, "pnvrs")] {
        let user = create_plain_user(&client, &admin, &team, tag).await;
        let before = now_seconds();

        let (status, served, body) = put(
            &client,
            base,
            Some(&user.token),
            // A **new** id twice: without the de-duplication the second insert of `notice-b`
            // collides with the first and the whole write is the 400.
            r#"["notice-b","notice-a","notice-b"]"#,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST);
        assert_eq!(
            body, br#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no newline"
        );
        let first = rows(&pool, &user.id).await;
        assert_eq!(first.len(), 2, "{base}");
        assert_eq!(first[0].0, "notice-a");
        assert_eq!(first[1].0, "notice-b");
        for (_, viewed, timestamp) in &first {
            assert_eq!(*viewed, 1, "{base}");
            assert!(
                (before..=now_seconds()).contains(timestamp),
                "{base}: a second-resolution stamp, got {timestamp}"
            );
        }

        // One again, twice in the list, plus a new one: the repeat counts once.
        let (status, _, _) = put(
            &client,
            base,
            Some(&user.token),
            r#"["notice-a","notice-c","notice-a"]"#,
        )
        .await;
        assert_eq!(status, 200, "{base}");
        let second = rows(&pool, &user.id).await;
        assert_eq!(
            second
                .iter()
                .map(|(id, viewed, _)| (id.as_str(), *viewed))
                .collect::<Vec<_>>(),
            vec![("notice-a", 2), ("notice-b", 1), ("notice-c", 1)],
            "{base}"
        );

        // `null` and `[]` are both a write of nothing — of no row at all, `""` included.
        let total = total_rows(&pool, &user.id).await;
        for body in ["null", "[]"] {
            let (status, _, response) = put(&client, base, Some(&user.token), body).await;
            assert_eq!(
                status,
                200,
                "{base} {body}: {}",
                String::from_utf8_lossy(&response)
            );
            assert_eq!(response, br#"{"status":"OK"}"#);
        }
        assert_eq!(
            rows(&pool, &user.id).await.len(),
            3,
            "{base}: nothing added"
        );
        assert_eq!(
            total_rows(&pool, &user.id).await,
            total,
            "{base}: no row of any id added"
        );
    }
}

/// The parse error for everything that is not a list of strings, and the session requirement.
#[tokio::test]
async fn a_body_that_is_not_a_list_of_strings_is_the_parse_error() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "pnvb").await;
    let user = create_plain_user(&client, &admin, &team, "pnvb").await;

    for body in ["{}", "[1]", "\"a\"", "", "[\"a\","] {
        let (go_status, _, go) = put(&client, GO, Some(&user.token), body).await;
        let (rs_status, served, rs) = put(&client, RUST, Some(&user.token), body).await;
        assert_eq!(
            go_status,
            400,
            "Go {body:?}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            400,
            "Rust {body:?}: {}",
            String::from_utf8_lossy(&rs)
        );
        assert!(served, "{body:?}: served here");
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
        assert_eq!(parsed["id"], "api.payload.parse.error", "{body:?}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/system/notices/view");
    }

    let (go_status, _, go) = put(&client, GO, None, "[]").await;
    let (rs_status, served, rs) = put(&client, RUST, None, "[]").await;
    assert_eq!((go_status, rs_status), (401, 401));
    assert!(served);
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/system/notices/view");
}
