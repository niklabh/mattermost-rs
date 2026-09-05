//! Cross-server parity for `POST /api/v4/users/search` — `searchUsers`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity users_search
//! ```
//!
//! # The validation order is the wire
//!
//! `limit` is defaulted **before** `term` is checked, so `{}` is the term 400 and never the limit
//! one; and `limit` is range-checked **last**, after the permission checks, so a body with both a
//! bad team and a bad limit is the 403. [`the_validation_order_is_go_s`].
//!
//! All three 400s share one id (`api.context.invalid_body_param.app_error`) and differ only in
//! the parameter name, which `AppError.params` never puts on the wire — so a client cannot tell
//! `props` from `term` from `limit`.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, go_minted_token, post_both_raw, purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/users/search";

struct Fixture {
    team_id: String,
    /// A team the plain user is not in.
    other_team_id: String,
    /// Alive, in `team_id`, and carrying a planted email whose local part appears nowhere else.
    alive: String,
    /// Deactivated, so it is a result only with `allow_inactive`.
    deactivated: String,
    /// The username stem both share.
    stem: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "usrsearch").await;
            let other_team_id = create_team(client, token, "usrsearchb").await;

            let alive = create_plain_user(client, token, &team_id, "usrsearch").await;
            let doomed = create_plain_user(client, token, &team_id, "usrsearchdead").await;

            let alive_name = username_of(client, token, &alive.id).await;
            let dead_name = username_of(client, token, &doomed.id).await;

            // `Email` is a searchable column only when `AllowEmails` is on, and every fixture
            // user's email is derived from its username — so without a planted address the email
            // arm of the search clause matches exactly when the username arm does, and is dead.
            plant_email(&alive.id, "mmrsparityzebra@example.com").await;

            delete_plain_user(client, token, &doomed.id).await;

            Fixture {
                team_id,
                other_team_id,
                alive: alive_name,
                deactivated: dead_name,
                stem: "mmrsplainusrsearch".to_owned(),
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

async fn plant_email(user_id: &str, email: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    let _ = sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
        .bind(user_id)
        .bind(email)
        .execute(&pool)
        .await;
}

fn body(props: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&props).expect("JSON")
}

fn names(raw: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .expect("JSON")
        .as_array()
        .expect("an array")
        .iter()
        .map(|u| u["username"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The default shape, byte for byte, and username-ordered.
#[tokio::test]
async fn a_plain_search_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(serde_json::json!({ "term": f.stem })),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert!(
        !go.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` appends no newline: {}",
        String::from_utf8_lossy(&go)
    );

    let found = names(&go);
    assert_eq!(
        found,
        vec![f.alive.clone()],
        "the deactivated one needs `allow_inactive`: {found:?}"
    );
}

/// `allow_inactive` is the only thing that admits a deactivated account.
#[tokio::test]
async fn allow_inactive_admits_the_deactivated_user() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(serde_json::json!({ "term": f.stem, "allow_inactive": true })),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );

    let mut found = names(&go);
    found.sort_unstable();
    assert_eq!(found, vec![f.alive.clone(), f.deactivated.clone()]);
}

/// `Email` is a searchable column, and a planted address is the only way to prove it.
#[tokio::test]
async fn an_email_only_term_matches_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(serde_json::json!({ "term": "mmrsparityzebra" })),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        names(&go),
        vec![f.alive.clone()],
        "`zebra` is in the email and in no username, nickname or name"
    );
}

/// `team_id` scopes the search to that team's members.
#[tokio::test]
async fn the_team_id_scopes_the_search() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (team_id, expected) in [(&f.team_id, 1usize), (&f.other_team_id, 0)] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(
            &client,
            &token,
            PATH,
            &body(serde_json::json!({ "term": f.stem, "team_id": team_id })),
        )
        .await;
        assert_eq!(go_status, 200, "{team_id}");
        assert_eq!(rs_status, go_status, "{team_id}");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{team_id} must be byte-identical"
        );
        assert_eq!(names(&go).len(), expected, "{team_id}");
    }
}

/// `limit` caps the result, and its range check runs **after** the permission checks.
#[tokio::test]
async fn the_validation_order_is_go_s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // `{}` is the *term* 400, not the limit one: `limit` is defaulted before `term` is read.
    for raw in [
        serde_json::json!({}),
        serde_json::json!({ "term": "" }),
        serde_json::json!({ "term": f.stem, "limit": -1 }),
        serde_json::json!({ "term": f.stem, "limit": 1001 }),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, &body(raw.clone())).await;
        assert_eq!(go_status, 400, "{raw}");
        assert_eq!(rs_status, go_status, "{raw}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &raw.to_string());
        assert_eq!(
            go["id"], "api.context.invalid_body_param.app_error",
            "{raw}"
        );
    }

    // A bad limit *and* a team the caller cannot see: the permission check wins.
    let props = serde_json::json!({
        "term": f.stem,
        "team_id": f.other_team_id,
        "limit": -1,
    });
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(props)).await;
    assert_eq!(go_status, 403, "the team check runs before the limit range");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // And `limit` does cap.
    let ((_, go), (_, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(serde_json::json!({ "term": f.stem, "allow_inactive": true, "limit": 1 })),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    assert_eq!(names(&go).len(), 1, "limit 1 returns one");
}

/// A body that is not an object, and the `null` that decodes to the zero value.
#[tokio::test]
async fn a_non_object_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for raw in [
        &b"notjson"[..],
        // serde would build the struct from this positionally and *search*.
        &br#"["slice"]"#[..],
        &b"5"[..],
        // Decodes to the zero value, so it lands on the empty-term branch — same id.
        &b"null"[..],
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}] must be rejected by Go");
        assert_eq!(rs_status, go_status, "[{shown}]: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(
            go["id"], "api.context.invalid_body_param.app_error",
            "for [{shown}]"
        );
    }
}

/// Every field that picks a different store query is handed to Go.
#[tokio::test]
async fn the_query_shaping_fields_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for field in [
        "not_in_team_id",
        "in_channel_id",
        "not_in_channel_id",
        "in_group_id",
        "not_in_group_id",
        "without_team",
        "group_constrained",
        "role",
        "roles",
        "channel_roles",
        "team_roles",
    ] {
        let mut props = serde_json::json!({ "term": f.stem });
        props[field] = match field {
            "without_team" | "group_constrained" => serde_json::json!(false),
            "roles" | "channel_roles" | "team_roles" => serde_json::json!([]),
            _ => serde_json::json!(""),
        };
        let rs = client
            .post(format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .body(body(props))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "a body carrying `{field}` must be forwarded, even at its zero value"
        );
    }
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
            .body(body(serde_json::json!({ "term": "x" })))
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
