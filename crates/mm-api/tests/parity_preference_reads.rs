//! Cross-server parity for the three preference reads:
//!
//! * `GET /api/v4/users/{user_id}/preferences`
//! * `GET /api/v4/users/{user_id}/preferences/{category}`
//! * `GET /api/v4/users/{user_id}/preferences/{category}/name/{preference_name}`
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_preference_reads
//! ```
//!
//! Other sessions' tests write preferences onto the shared fixture user while these run, so the
//! success cases compare Go against Rust for the same request at the same moment
//! (`fetch_both_stable`) rather than asserting contents.

mod common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

const CATEGORY: &str = "display_settings";
// Written by this suite through Go before it is read, so the category-and-name route has a row
// no other suite flips.
const NAME: &str = "mmrs_parity_read_name";

fn prefs_path(user: &str) -> String {
    format!("/api/v4/users/{user}/preferences")
}

async fn put_through_go(client: &reqwest::Client, token: &str, user_id: &str, value: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/preferences"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": user_id,
            "category": CATEGORY,
            "name": NAME,
            "value": value,
        }]))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "seeding the preference failed");
}

/// Best-effort: the shared purge removes `mmrsplain%` users but not their preference rows.
async fn purge_plain_user_preferences(user_id: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return;
    };
    let _ = sqlx::query("DELETE FROM preferences WHERE userid = $1")
        .bind(user_id)
        .execute(&pool)
        .await;
}

/// The three reads, `me` and explicit id, byte for byte — including the encoder's newline.
#[tokio::test]
async fn the_three_reads_match_for_me_and_for_the_explicit_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    put_through_go(&client, &token, me, "seeded").await;

    for user in ["me", me] {
        let base = prefs_path(user);
        for path in [
            base.clone(),
            format!("{base}/{CATEGORY}"),
            format!("{base}/{CATEGORY}/name/{NAME}"),
        ] {
            let (go, rs) = fetch_both_stable(&client, &token, &path).await;
            assert_eq!(
                String::from_utf8_lossy(&go),
                String::from_utf8_lossy(&rs),
                "{path}"
            );
            assert!(go.ends_with(b"\n"), "{path}: Go's encoder newline");
        }
    }

    // The single read is an object, the other two are arrays, and the seeded row is in all three.
    let (_, all) = fetch_both_stable(&client, &token, &prefs_path(me)).await;
    let all: Vec<serde_json::Value> = serde_json::from_slice(&all).expect("an array");
    assert!(all.iter().any(|p| p["name"] == NAME));
    let (_, one) = fetch_both_stable(
        &client,
        &token,
        &format!("{}/{CATEGORY}/name/{NAME}", prefs_path(me)),
    )
    .await;
    let one: serde_json::Value = serde_json::from_slice(&one).expect("an object");
    assert_eq!(one["value"], "seeded");
    assert_eq!(one["user_id"], me);
}

/// A fresh user already carries default preferences, so the only way to reach `GetAll` with
/// zero rows is to delete them — after which Go encodes a nil slice: `null`, not `[]`.
#[tokio::test]
async fn an_emptied_user_reads_back_as_null() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "prefnull").await;

    let existing: serde_json::Value = client
        .get(format!("{GO}{}", prefs_path(&plain.id)))
        .header("Authorization", format!("Bearer {}", plain.token))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("decodes");
    assert!(
        existing.as_array().is_some_and(|a| !a.is_empty()),
        "the fixture assumes a new user has default preferences; it no longer does"
    );
    let deleted = client
        .post(format!("{GO}{}/delete", prefs_path(&plain.id)))
        .header("Authorization", format!("Bearer {}", plain.token))
        .json(&existing)
        .send()
        .await
        .expect("Go answers");
    assert_eq!(deleted.status(), 200, "deleting the defaults failed");

    let (go, rs) = fetch_both_stable(&client, &plain.token, &prefs_path("me")).await;
    assert_eq!(go, b"null\n", "Go encodes a nil slice");
    assert_eq!(rs, go);

    // And by category, an emptied user is the 404 from the app layer.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &plain.token,
        &format!("{}/{CATEGORY}", prefs_path("me")),
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "emptied category");
    assert_eq!(
        go["id"],
        "api.preference.preferences_category.get.app_error"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
    purge_plain_user_preferences(&plain.id).await;
}

/// The refusals, each with its own status and id, and each chosen by a different layer.
#[tokio::test]
async fn the_refusals_match_by_status_and_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let base = prefs_path("me");

    let cases: &[(String, u16, &str)] = &[
        // RequireCategory: uppercase, one character, and edge underscores route in mux and 400.
        (
            format!("{base}/Display_Settings"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            format!("{base}/a"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            format!("{base}/__"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            format!("{base}/_display"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        // RequirePreferenceName, with a category that passes — the chain stops at the name.
        (
            format!("{base}/{CATEGORY}/name/Nope"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            format!("{base}/{CATEGORY}/name/x"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        // A category that is not valid AND a name that is: the earlier segment names the 400.
        (
            format!("{base}/Bad/name/ok_name"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        // An empty category is the app layer's 404 ...
        (
            format!("{base}/mmrs_no_such_cat"),
            404,
            "api.preference.preferences_category.get.app_error",
        ),
        // ... and `delete` reaches the category handler on GET, as gorilla falls past the POST route.
        (
            format!("{base}/delete"),
            404,
            "api.preference.preferences_category.get.app_error",
        ),
        // ... while a missing name is a **400**, because Go's store wraps ErrNoRows as a plain error.
        (
            format!("{base}/{CATEGORY}/name/mmrs_no_such_name"),
            400,
            "app.preference.get.app_error",
        ),
        // A user id that passes the mux but not IsValidId.
        (
            prefs_path("nope"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            format!("{}/{CATEGORY}", prefs_path("nope")),
            400,
            "api.context.invalid_url_param.app_error",
        ),
    ];

    for (path, status, id) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, path).await;
        assert_eq!(
            go_status, *status,
            "{path}: Go's status is not what this test assumes"
        );
        assert_eq!(rs_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(go["id"], *id, "{path}");
    }
}

/// A hyphen is inside `RequireCategory`'s alphabet and outside the mux class, so Go never routes
/// it — the answer is the mux 404, and ours must be Go's own, via the proxy.
#[tokio::test]
async fn a_hyphenated_segment_is_forwarded_for_gos_mux_404() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let base = prefs_path("me");

    for path in [
        format!("{base}/display-settings"),
        format!("{base}/{CATEGORY}/name/use-military-time"),
        format!("{base}/display-settings/name/ok_name"),
    ] {
        let get = async |base_url: &str| {
            let response = client
                .get(format!("{base_url}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("reachable");
            let served_by = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let status = response.status().as_u16();
            let body: serde_json::Value = response.json().await.expect("JSON");
            (status, served_by, body)
        };
        let (go_status, _, go_body) = get(GO).await;
        let (rs_status, served_by, rs_body) = get(RUST).await;
        assert_eq!(go_status, 404, "{path}");
        assert_eq!(rs_status, 404, "{path}");
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{path}: must be forwarded"
        );
        assert_eq!(go_body["id"], "api.context.404.app_error", "{path}");
        assert_eq!(rs_body, go_body, "{path}");
    }
}

/// `SessionHasPermissionToUser`: a plain user reading the admin's preferences is refused with
/// `edit_other_users` on every one of the three routes; the admin reading the plain user's is
/// not. The refusal has to come before the read — both servers answer the same 403 whether or
/// not the category exists.
#[tokio::test]
async fn reading_another_users_preferences_needs_edit_other_users() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let admin_id = logged_in_user_id();
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "prefperm").await;

    let base = prefs_path(admin_id);
    for path in [
        base.clone(),
        format!("{base}/{CATEGORY}"),
        format!("{base}/mmrs_no_such_cat"),
        format!("{base}/{CATEGORY}/name/{NAME}"),
        format!("{base}/{CATEGORY}/name/mmrs_no_such_name"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &plain.token, &path).await;
        assert_eq!((go_status, rs_status), (403, 403), "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.permissions.app_error", "{path}");
    }

    // The admin reading the plain user: allowed, and identical.
    let plain_base = prefs_path(&plain.id);
    for path in [plain_base.clone(), format!("{plain_base}/tutorial_step")] {
        let (go, rs) = fetch_both_stable(&client, &admin, &path).await;
        assert_eq!(go, rs, "{path}");
        assert!(!go.is_empty());
    }

    delete_plain_user(&client, &admin, &plain.id).await;
    purge_plain_user_preferences(&plain.id).await;
}
