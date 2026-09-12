//! Cross-server parity for `GET /api/v4/terms_of_service` — `getLatestTermsOfService`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity terms_of_service
//! ```
//!
//! # The fixture is fixed, and that is the point
//!
//! Go caches the answer under the key `"latest"` (localcachelayer/terms_of_service_layer.go:47)
//! and invalidates it only on `Save` — which is licence-gated here, so the table can only be
//! written by hand and a direct write is invisible to that cache for as long as it holds a row.
//!
//! So this suite's rows have **fixed ids, fixed timestamps and fixed text**. Every run plants the
//! same two rows, so whatever Go cached from an earlier run is byte-identical to what the table
//! now holds. Changing the text below will make **one** run disagree, and the fix is to wait out
//! the cache rather than to add a tiebreak.
//!
//! # The 404 is not tested here
//!
//! An empty table is `app.terms_of_service.get.no_rows.app_error`, and reaching it means deleting
//! every row — which would leave Go's cache serving one that no longer exists for the rest of the
//! run. The branch has a unit test in `mm_app::terms_of_service` instead. Go does **not** cache
//! the miss (`if allowFromCache && err == nil`), so the 404 is a live query on both servers; it is
//! the *success* that is sticky.

use crate::common;

use common::{RUST, client, fetch_both_raw, fetch_both_stable, go_minted_token, stack_enabled};

const PATH: &str = "/api/v4/terms_of_service";

/// The newest of the two planted revisions — the one the route must answer with.
const LATEST_ID: &str = "mmrstos00000000000000newer";
const OLDER_ID: &str = "mmrstos00000000000000older";
const LATEST_TEXT: &str = "mmrs parity terms, revision two";
const OLDER_TEXT: &str = "mmrs parity terms, revision one";
/// Fixed, and the newer one is **later**, which is the only thing `ORDER BY CreateAt DESC` reads.
const OLDER_CREATE_AT: i64 = 1_788_000_000_000;
const LATEST_CREATE_AT: i64 = 1_788_000_000_001;

static PLANTED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn plant() {
    PLANTED
        .get_or_init(|| async {
            let url = std::env::var("DATABASE_URL").expect(
                "DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it",
            );
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(5))
                .connect(&url)
                .await
                .expect("the shared database is reachable");

            // `ON CONFLICT DO NOTHING` rather than delete-and-insert: the rows are identical every
            // run, so re-inserting them is a no-op, and *deleting* them — even for a moment —
            // would let a concurrent read cache a different answer.
            for (id, create_at, text) in [
                (OLDER_ID, OLDER_CREATE_AT, OLDER_TEXT),
                (LATEST_ID, LATEST_CREATE_AT, LATEST_TEXT),
            ] {
                sqlx::query(
                    "INSERT INTO termsofservice (id, createat, userid, text)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(id)
                .bind(create_at)
                .bind(common::logged_in_user_id())
                .bind(text)
                .execute(&pool)
                .await
                .expect("the terms of service row is written");
            }
        })
        .await;
}

/// The newest revision, byte for byte, with a trailing newline.
#[tokio::test]
async fn the_latest_revision_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    plant().await;

    let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a newline"
    );

    let terms: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert_eq!(
        terms["id"], LATEST_ID,
        "`ORDER BY CreateAt DESC LIMIT 1` — the *newer* row wins, not the first inserted"
    );
    assert_eq!(terms["text"], LATEST_TEXT);
    assert_eq!(terms["create_at"], LATEST_CREATE_AT);
    assert_eq!(
        terms.as_object().expect("an object").len(),
        4,
        "four fields, none omitted: {terms}"
    );
}

/// The **older** revision is in the table and is not the answer — so the ordering claim above is
/// about `ORDER BY` and not about there being one row.
#[tokio::test]
async fn the_older_revision_exists_and_is_not_returned() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    plant().await;

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set by scripts/parity.sh");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM termsofservice WHERE id = $1")
        .bind(OLDER_ID)
        .fetch_one(&pool)
        .await
        .expect("the guard query runs");
    assert_eq!(count, 1, "the older revision really is in the table");

    let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
    let terms: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert_ne!(terms["id"], OLDER_ID, "and it is not the one returned");
    assert_eq!(go, rs, "{PATH}");
}

/// A session is required and **nothing else** — no permission, no team, no membership. An
/// anonymous request is the ordinary session error.
#[tokio::test]
async fn a_session_is_required_and_no_permission_is() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    plant().await;

    // A plain user with no permissions at all gets the terms.
    let team = common::a_team_and_channel_the_user_is_in(&client, &token)
        .await
        .0;
    let plain = common::create_plain_user(&client, &token, &team, "tosread").await;
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &plain.token, PATH).await;
    assert_eq!(go_status, 200, "{PATH}: no permission is required");
    assert_eq!(rs_status, go_status, "{PATH}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );

    // And with no credentials at all, both refuse the same way.
    let anonymous = async |base: &str| {
        let response = client
            .get(format!("{base}{PATH}"))
            .send()
            .await
            .expect("answers");
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body reads").to_vec())
    };
    let (go_status, go) = anonymous(common::GO).await;
    let (rs_status, rs) = anonymous(RUST).await;
    assert_eq!(go_status, 401, "a session is required");
    assert_eq!(rs_status, go_status);
    common::assert_error_bodies_match_except_known_gaps(&go, &rs, PATH);
}

/// The `POST` beside this `GET` is now served too, and it must **not** publish.
///
/// `createTermsOfService` is licence-gated and this installation is unlicensed, so its whole
/// behaviour here is a 400 — which is asserted against Go in `parity::terms_of_service_writes`.
/// What this test guards is narrower and belongs beside the read: a `POST` that published would
/// move *this* suite's answer, since the new revision would be the latest. It asks for the
/// current latest, posts, and asks again.
#[tokio::test]
async fn the_post_beside_it_is_served_and_publishes_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    plant().await;

    let latest_id = async || -> String {
        let (_, body) = fetch_both_raw(&client, &token, PATH).await.0;
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        value["id"].as_str().unwrap_or_default().to_owned()
    };
    let before = latest_id().await;
    assert_eq!(before, LATEST_ID, "the fixture is in place");

    let ours = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "text": "mmrs" }))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "POST {PATH} is served here now"
    );
    assert_eq!(ours.status().as_u16(), 400, "and it is the licence refusal");

    assert_eq!(
        latest_id().await,
        before,
        "a refused publish must leave the table alone"
    );
}
