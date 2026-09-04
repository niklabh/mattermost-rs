//! Cross-server parity for `GET /api/v4/users/{user_id}/terms_of_service`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity user_terms_of_service
//! ```
//!
//! # The path parameter is read by the router and by nothing else
//!
//! The handler's first line is `userId := c.AppContext.Session().UserId`. There is no
//! `RequireUserId`, no `me` resolution, and no comparison against the segment — so this route
//! answers **your own** acceptance record whatever id is in the path. That looks like an
//! authorization hole and is the opposite of one: the route cannot disclose another user's
//! record because it never looks one up, and a port that "fixed" the parameter by honouring it
//! would create the hole it appears to have.
//!
//! # A 404 is the ordinary answer
//!
//! Team Edition cannot author a terms of service, so no account can accept one through the API
//! and the row has to be planted. On a stock server every caller gets the 404.

use crate::common;

use common::{
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, fetch_both,
    fetch_both_raw, go_minted_token, logged_in_user_id, plant_terms_of_service_row,
    purge_api_fixtures, stack_enabled,
};

const TOS_ID: &str = "mmrstosparityrowaaaaaaaaaa";

struct Fixture {
    /// Has a planted acceptance row.
    accepted_id: String,
    accepted_token: String,
    /// Has none, so it sees the 404 branch.
    absent_token: String,
    /// True when `DATABASE_URL` was available and the row could be planted.
    planted: bool,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;

            let accepted = create_plain_user(client, token, &team_id, "tosyes").await;
            let absent = create_plain_user(client, token, &team_id, "tosno").await;
            let planted = plant_terms_of_service_row(&accepted.id, TOS_ID).await;

            Fixture {
                accepted_id: accepted.id,
                accepted_token: accepted.token,
                absent_token: absent.token,
                planted,
            }
        })
        .await
}

#[tokio::test]
async fn an_accepted_record_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let path = format!("/api/v4/users/{}/terms_of_service", f.accepted_id);
    let (go, rs) = fetch_both(&client, &f.accepted_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "json.NewEncoder().Encode adds a trailing newline"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["user_id"], f.accepted_id.as_str());
    assert_eq!(parsed["terms_of_service_id"], TOS_ID);
    assert_eq!(parsed["create_at"], 1_700_000_000_000_i64);
}

/// The finding: the path segment is ignored. Three different ids, one answer — the caller's own.
#[tokio::test]
async fn the_path_id_is_ignored_and_the_session_decides() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let own = format!("/api/v4/users/{}/terms_of_service", f.accepted_id);
    let (expected, _) = fetch_both(&client, &f.accepted_token, &own).await;

    for other in [
        logged_in_user_id().to_owned(),
        "zzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
    ] {
        let path = format!("/api/v4/users/{other}/terms_of_service");
        let (go, rs) = fetch_both(&client, &f.accepted_token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&expected),
            "{path}: the segment is not read, so this is still the caller's own record"
        );
    }

    // And the mirror image: the *admin*, asking with the accepted user's id, gets its own
    // (absent) record rather than that user's. Without this half the assertion above would hold
    // for a port that returned the path user's record whenever it existed.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &own).await;
    assert_eq!(
        go_status, 404,
        "the admin has no record of its own, and the path id is not consulted"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &own);
}

/// The common case on a stock server.
#[tokio::test]
async fn no_record_is_a_404_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/{}/terms_of_service", logged_in_user_id());
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.absent_token, &path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"], "app.user_terms_of_service.get_by_user.no_rows.app_error",
        "the 404 id carries `no_rows.`, one word apart from the 500's"
    );
}

/// A segment outside gorilla's `[A-Za-z0-9]+` never reaches a handler on either server, and one
/// inside it but the wrong length reaches ours — where it is still ignored, so it is a 200 or a
/// 404 rather than the 400 every other `{user_id}` route answers.
#[tokio::test]
async fn a_short_id_is_not_a_400_because_nothing_validates_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let path = "/api/v4/users/short/terms_of_service";
    let (go, rs) = fetch_both(&client, &f.accepted_token, path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}: there is no RequireUserId on this route"
    );

    // Its sibling on the same segment *does* validate, which is what makes the 200 above a
    // statement about this route rather than about the router.
    let sibling = "/api/v4/users/short/status";
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, sibling).await;
    assert_eq!(go_status, 400, "{sibling} calls RequireUserId");
    assert_eq!(rs_status, go_status);
}

/// `POST` on the same path is `saveUserTermsOfService` and must still be Go's.
#[tokio::test]
async fn the_post_on_the_same_path_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/{}/terms_of_service", f.accepted_id);
    let response = client
        .post(format!("{}{path}", common::RUST))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "termsOfServiceId": TOS_ID, "accepted": true }))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "only GET is migrated"
    );
}
