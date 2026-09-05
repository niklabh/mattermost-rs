//! Cross-server parity for `POST /api/v4/users/usernames` — `getUsersByNames`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity users_by_names
//! ```
//!
//! # A name that matches nothing is simply absent
//!
//! Nothing here validates a username, on the list or on its members, and there is no not-found:
//! a request for five names can answer with two, and the caller cannot tell "no such user" from
//! "not allowed to see them". [`unknown_names_are_dropped_rather_than_refused`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, go_minted_token, logged_in_user_id, post_both_raw, purge_api_fixtures,
    stack_enabled,
};

const PATH: &str = "/api/v4/users/usernames";

struct Fixture {
    /// Alive, and a member of the fixture's team.
    alive: String,
    /// Created, then deactivated — this route carries no `DeleteAt` filter, so it still answers.
    deactivated: String,
    admin_username: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "bynames").await;

            let alive = create_plain_user(client, token, &team_id, "bynames").await;
            let doomed = create_plain_user(client, token, &team_id, "bynamesdead").await;

            let alive_name = username_of(client, token, &alive.id).await;
            let dead_name = username_of(client, token, &doomed.id).await;
            let admin_username = username_of(client, token, logged_in_user_id()).await;

            // `GetProfilesByUsernames` has no deletion filter at all, so a deactivated account is
            // returned like any other — which is what lets a client render an old mention. The
            // branch is dead until a fixture row carries `DeleteAt != 0`.
            delete_plain_user(client, token, &doomed.id).await;

            Fixture {
                alive: alive_name,
                deactivated: dead_name,
                admin_username,
                plain_token: alive.token,
            }
        })
        .await
}

async fn username_of(client: &reqwest::Client, token: &str, user_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let user: serde_json::Value = response.json().await.expect("the user decodes");
    user["username"].as_str().expect("a username").to_owned()
}

fn body(names: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&names).expect("a JSON array")
}

/// The array is byte-identical, and ordered by username rather than by the request.
#[tokio::test]
async fn the_list_is_byte_identical_and_username_ordered() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Deliberately not in username order, and `SortedArrayFromJSON` plus the query's `ORDER BY`
    // both put it back.
    let asked = body(&[&f.deactivated, &f.admin_username, &f.alive]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, &asked).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert!(
        !go.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` appends no newline, unlike getUser: {}",
        String::from_utf8_lossy(&go)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let names: Vec<&str> = parsed
        .as_array()
        .expect("an array")
        .iter()
        .map(|u| u["username"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names.len(), 3, "all three exist: {parsed}");
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "ORDER BY Users.Username ASC: {parsed}");
    assert!(
        names.contains(&f.deactivated.as_str()),
        "a deactivated account is still answered — there is no DeleteAt filter: {parsed}"
    );
}

/// Names that match nobody are dropped; the request is not refused.
#[tokio::test]
async fn unknown_names_are_dropped_rather_than_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // One real name, one that names nobody, and one that is not a valid username at all —
    // nothing here validates the shape, so both misses are the same absence.
    let asked = body(&[&f.alive, "mmrs-parity-nobody", "NOT A USERNAME"]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, &asked).await;
    assert_eq!(go_status, 200, "no validation, no not-found");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed.as_array().expect("an array").len(), 1, "{parsed}");
    assert_eq!(parsed[0]["username"], f.alive.as_str());
}

/// The two 400s, in Go's order: the decode first, then the empty list.
#[tokio::test]
async fn a_bad_body_and_an_empty_list_are_the_two_400s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for (raw, id) in [
        (&b"not json"[..], "api.payload.parse.error"),
        (&b"{\"a\":1}"[..], "api.payload.parse.error"),
        (&b"[1,2]"[..], "api.payload.parse.error"),
        (&b"[]"[..], "api.context.invalid_body_param.app_error"),
        // `SortedArrayFromJSON` reduces `null` to zero names without an error, so it lands on
        // the *second* 400 rather than the first.
        (&b"null"[..], "api.context.invalid_body_param.app_error"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "{shown} must be rejected by Go");
        assert_eq!(rs_status, go_status, "{shown}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], id, "for the body {shown}");
    }
}

/// A plain caller gets the same array, sanitised the same way.
#[tokio::test]
async fn a_plain_caller_sees_the_non_admin_view() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let asked = body(&[&f.admin_username]);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.plain_token, PATH, &asked).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert!(
        parsed[0].get("notify_props").is_none(),
        "sanitizeProfiles(users, false) for a non-admin: {parsed}"
    );
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{PATH}"))
            .json(&serde_json::json!(["whoever"]))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{PATH}");
    }
}

/// Every other method on this path is Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {PATH} must be forwarded"
        );
    }
}
