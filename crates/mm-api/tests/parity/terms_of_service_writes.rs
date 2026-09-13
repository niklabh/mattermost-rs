//! Cross-server parity for `POST /api/v4/terms_of_service` and
//! `POST /api/v4/users/{user_id}/terms_of_service`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::terms_of_service_writes
//! ```
//!
//! # `createTermsOfService` is one refusal on this deployment, and that is the whole route
//!
//! `manage_system` first, then `license == nil || !*license.Features.CustomTermsOfService` — a
//! **400**, not the 501 the content-flagging and channel-bookmark families give for a licence.
//! Unlicensed is the only state this server can read, so the gate never opens here and the body
//! is never parsed. Everything past it (the empty-text 400, the compare-with-the-latest branch,
//! the insert) is unreachable over HTTP and is covered by unit and store tests instead; see
//! [D-382].
//!
//! # `saveUserTermsOfService` writes, and its path parameter is decoration
//!
//! The handler reads `c.AppContext.Session().UserId` and never looks at `{user_id}` — the same
//! shape as the `GET` beside it, and the same reason it is not a hole: it cannot name a user it
//! never looks up. [`the_path_user_is_ignored_on_both_servers`] measures that by accepting
//! through *another* user's path and reading the record back from the caller's own.
//!
//! # The fixture plants a revision rather than publishing one
//!
//! Team Edition cannot author a terms of service at all, so the only way to reach the accept path
//! is a direct insert. The planted row's `CreateAt` is deliberately **older** than the one
//! `parity::terms_of_service` plants, so `GET /api/v4/terms_of_service` — which answers with the
//! latest — is unaffected, and so is Go's `"latest"` cache.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    go_minted_token, purge_api_fixtures, stack_enabled,
};

/// The revision this suite plants. Fixed, so re-running is idempotent.
const TOS_ID: &str = "mmrstoswrite00000000000001";
/// Older than `parity::terms_of_service`'s rows (1_788_000_000_000 and one later), so nothing
/// this suite plants can become "the latest" and move that suite's answer.
const TOS_CREATE_AT: i64 = 1_700_000_000_000;

/// # One account per writing test
///
/// The tests in this binary run **in parallel** and the acceptance table is keyed on the user, so
/// two tests accepting and rejecting as the same account race on one row — `accepted:false` from
/// one lands between the other's `UPDATE` and its `INSERT`, and the insert then collides with the
/// primary key for a 500. Measured, not theorised: it is what the first run of
/// [`the_path_user_is_ignored_on_both_servers`] did. Each writing test therefore has an account
/// of its own.
struct Fixture {
    /// A second account, so the ignored path parameter can be pointed at somebody real. Never
    /// written to — that it stays empty is the assertion.
    other_id: String,
    /// `accepting_and_then_rejecting_round_trips_through_the_table`'s account.
    actor_id: String,
    actor_token: String,
    /// `the_path_user_is_ignored_on_both_servers`' account.
    path_actor_id: String,
    path_actor_token: String,
    /// False when `DATABASE_URL` was unavailable, so the accept tests skip rather than fail.
    planted: bool,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;

            let actor = create_plain_user(client, token, &team_id, "toswr").await;
            let path_actor = create_plain_user(client, token, &team_id, "toswp").await;
            let other = create_plain_user(client, token, &team_id, "toswo").await;
            let planted = plant_revision().await;

            Fixture {
                other_id: other.id,
                actor_id: actor.id,
                actor_token: actor.token,
                path_actor_id: path_actor.id,
                path_actor_token: path_actor.token,
                planted,
            }
        })
        .await
}

/// Insert the revision this suite accepts, if the database is reachable.
async fn plant_revision() -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query(
        "INSERT INTO termsofservice (id, createat, userid, text)
         VALUES ($1, $2, 'mmrstoswriteauthor00000001', 'mmrs parity accept fixture')
         ON CONFLICT (id) DO UPDATE SET createat = $2",
    )
    .bind(TOS_ID)
    .bind(TOS_CREATE_AT)
    .execute(&pool)
    .await
    .expect("plants the terms-of-service revision");
    true
}

/// The acceptance row for `user_id`, read straight from the table.
async fn acceptance(user_id: &str) -> Option<(String, i64)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()?;
    sqlx::query_as::<_, (String, i64)>(
        "SELECT COALESCE(termsofserviceid, ''), COALESCE(createat, 0)
           FROM usertermsofservice WHERE userid = $1",
    )
    .bind(user_id)
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten()
}

/// `(status, body, x-mmrs-served-by)` for one POST.
async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} POST {path} is unreachable: {e}"));
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

// ---------------------------------------------------------------------------------------------
// createTermsOfService
// ---------------------------------------------------------------------------------------------

/// The licence refusal, and the fact that it **precedes the body**.
///
/// A body that is not JSON at all gets the same 400 as a perfectly good one, because
/// `createTermsOfService` never reaches `MapFromJSON`. A port that parsed first would answer
/// `empty_text` — a different id — to the same request.
#[tokio::test]
async fn publishing_terms_is_refused_for_the_licence_before_the_body_is_read() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (context, body) in [
        ("a good body", r#"{"text":"some terms"}"#),
        ("an empty text", r#"{"text":""}"#),
        ("no text key", r#"{}"#),
        ("not json at all", "{"),
        ("an array", "[1,2,3]"),
        ("an empty body", ""),
    ] {
        let (go_status, go_body, _) =
            post(&client, GO, &token, "/api/v4/terms_of_service", body).await;
        let (rs_status, rs_body, served_by) =
            post(&client, RUST, &token, "/api/v4/terms_of_service", body).await;

        assert_eq!(served_by.as_deref(), Some("rust"), "{context}");
        assert_eq!(
            go_status,
            400,
            "{context}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, 400, "{context}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);

        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
        assert_eq!(
            rs["id"], "api.create_terms_of_service.custom_terms_of_service_disabled.app_error",
            "{context}: the licence refusal, not a body error"
        );
    }
}

/// `manage_system` is checked **first**, so a non-admin never learns the feature is licensed.
#[tokio::test]
async fn a_non_admin_is_refused_for_the_permission_not_the_licence() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin).await;

    let body = r#"{"text":"some terms"}"#;
    let (go_status, go_body, _) = post(
        &client,
        GO,
        &fixture.actor_token,
        "/api/v4/terms_of_service",
        body,
    )
    .await;
    let (rs_status, rs_body, served_by) = post(
        &client,
        RUST,
        &fixture.actor_token,
        "/api/v4/terms_of_service",
        body,
    )
    .await;

    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 403, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        403,
        "rust: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a non-admin publishing terms");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(rs["id"], "api.context.permissions.app_error");
}

// ---------------------------------------------------------------------------------------------
// saveUserTermsOfService
// ---------------------------------------------------------------------------------------------

/// The two type assertions and the unknown-revision 404, compared byte for byte.
#[tokio::test]
async fn the_accept_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin).await;
    let path = format!("/api/v4/users/{}/terms_of_service", fixture.actor_id);

    for (context, body, expected_id) in [
        (
            "no termsOfServiceId",
            r#"{"accepted":true}"#,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "a non-string termsOfServiceId",
            r#"{"termsOfServiceId":5,"accepted":true}"#,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "no accepted",
            r#"{"termsOfServiceId":"mmrstoswrite00000000000001"}"#,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "a string accepted",
            r#"{"termsOfServiceId":"mmrstoswrite00000000000001","accepted":"true"}"#,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "a body that is not JSON",
            "{",
            "api.context.invalid_body_param.app_error",
        ),
        (
            "an empty body",
            "",
            "api.context.invalid_body_param.app_error",
        ),
        (
            "an unknown revision",
            r#"{"termsOfServiceId":"zzzzzzzzzzzzzzzzzzzzzzzzzz","accepted":true}"#,
            "app.terms_of_service.get.no_rows.app_error",
        ),
        (
            "an empty revision id",
            r#"{"termsOfServiceId":"","accepted":true}"#,
            "app.terms_of_service.get.no_rows.app_error",
        ),
    ] {
        let (go_status, go_body, _) = post(&client, GO, &fixture.actor_token, &path, body).await;
        let (rs_status, rs_body, served_by) =
            post(&client, RUST, &fixture.actor_token, &path, body).await;

        assert_eq!(served_by.as_deref(), Some("rust"), "{context}");
        assert_eq!(
            rs_status,
            go_status,
            "{context}\n  go:   {}\n  rust: {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
        assert_eq!(rs["id"], expected_id, "{context}");
    }
}

/// Accepting writes the row, `ReturnStatusOK` has no trailing newline, and the already-migrated
/// `GET` on both servers reads back what this server wrote.
#[tokio::test]
async fn accepting_and_then_rejecting_round_trips_through_the_table() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin).await;
    if !fixture.planted {
        eprintln!("skipping: DATABASE_URL is unset, so no revision could be planted");
        return;
    }
    let path = format!("/api/v4/users/{}/terms_of_service", fixture.actor_id);
    let accept = format!(r#"{{"termsOfServiceId":"{TOS_ID}","accepted":true}}"#);
    let reject = format!(r#"{{"termsOfServiceId":"{TOS_ID}","accepted":false}}"#);

    let (status, body, served_by) = post(&client, RUST, &fixture.actor_token, &path, &accept).await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        body, br#"{"status":"OK"}"#,
        "ReturnStatusOK has no trailing newline"
    );

    let row = acceptance(&fixture.actor_id)
        .await
        .expect("the acceptance row is there");
    assert_eq!(row.0, TOS_ID);
    assert_ne!(row.1, 0, "PreSave stamped CreateAt");

    // Both servers report it the same way through the migrated GET.
    for base in [GO, RUST] {
        let read: serde_json::Value = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {}", fixture.actor_token))
            .send()
            .await
            .expect("the server answers")
            .json()
            .await
            .expect("the record decodes");
        assert_eq!(read["terms_of_service_id"], TOS_ID, "{base}");
        assert_eq!(read["user_id"], fixture.actor_id.as_str(), "{base}");
        assert_eq!(read["create_at"], serde_json::Value::from(row.1), "{base}");
    }

    // Accepting again is an UPDATE, not a second row, and it moves `CreateAt`.
    let (status, _, _) = post(&client, RUST, &fixture.actor_token, &path, &accept).await;
    assert_eq!(status, 200);
    let again = acceptance(&fixture.actor_id)
        .await
        .expect("still exactly one row");
    assert_eq!(again.0, TOS_ID);
    assert!(
        again.1 >= row.1,
        "PreSave overwrites CreateAt on every acceptance"
    );

    // Rejecting the *other* revision removes nothing: the delete matches on both columns.
    let other_reject = r#"{"termsOfServiceId":"zzzzzzzzzzzzzzzzzzzzzzzzzz","accepted":false}"#;
    let (status, body, _) = post(&client, RUST, &fixture.actor_token, &path, other_reject).await;
    assert_eq!(
        status,
        404,
        "the revision lookup runs before the branch: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        acceptance(&fixture.actor_id).await.is_some(),
        "the acceptance survives a rejection of a different revision"
    );

    // Rejecting the accepted one removes the row, and the GET goes back to its 404.
    let (status, body, served_by) = post(&client, RUST, &fixture.actor_token, &path, &reject).await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(
        acceptance(&fixture.actor_id).await.is_none(),
        "accepted:false is a DELETE"
    );

    let status = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {}", fixture.actor_token))
        .send()
        .await
        .expect("the server answers")
        .status()
        .as_u16();
    assert_eq!(status, 404, "no acceptance means the 404 branch again");

    // Rejecting when there is nothing to reject is a **success**, not a 404: the delete never
    // checks how many rows it removed.
    let (status, _, _) = post(&client, RUST, &fixture.actor_token, &path, &reject).await;
    assert_eq!(status, 200, "a rejection with no row is still OK");
}

/// `{user_id}` is read by the router and by nothing else: accepting through somebody else's path
/// records the **caller's** acceptance and leaves theirs untouched.
#[tokio::test]
async fn the_path_user_is_ignored_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fixture = fixture(&client, &admin).await;
    if !fixture.planted {
        eprintln!("skipping: DATABASE_URL is unset, so no revision could be planted");
        return;
    }

    // Point the path at the *other* user and accept as the actor.
    let someone_elses = format!("/api/v4/users/{}/terms_of_service", fixture.other_id);
    let accept = format!(r#"{{"termsOfServiceId":"{TOS_ID}","accepted":true}}"#);
    let (status, body, served_by) = post(
        &client,
        RUST,
        &fixture.path_actor_token,
        &someone_elses,
        &accept,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

    assert!(
        acceptance(&fixture.other_id).await.is_none(),
        "the user named in the path must not have accepted anything"
    );
    assert_eq!(
        acceptance(&fixture.path_actor_id).await.map(|row| row.0),
        Some(TOS_ID.to_owned()),
        "the caller accepted, whatever the path said"
    );

    // An id that names nobody at all works the same way, on both servers.
    let nowhere = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/terms_of_service";
    let (go_status, _, _) = post(&client, GO, &fixture.path_actor_token, nowhere, &accept).await;
    let (rs_status, _, served_by) =
        post(&client, RUST, &fixture.path_actor_token, nowhere, &accept).await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);

    // Tidy up, so the acceptance does not outlive the suite in a shared table.
    let reject = format!(r#"{{"termsOfServiceId":"{TOS_ID}","accepted":false}}"#);
    let path = format!("/api/v4/users/{}/terms_of_service", fixture.path_actor_id);
    post(&client, RUST, &fixture.path_actor_token, &path, &reject).await;
}
