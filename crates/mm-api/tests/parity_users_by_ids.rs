//! Cross-server parity for `POST /api/v4/users/ids`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_users_by_ids
//! ```
//!
//! # Why the lists are compared as sets
//!
//! Go serves this route through `LocalCacheUserStore.GetProfileByIds`: cache hits come back
//! first, **in request order**, and only the misses arrive from the database sorted by username.
//! The order on Go's wire therefore depends on which users were asked for recently by anyone —
//! the port always answers in the query's `Username ASC`. Every comparison below sorts both
//! lists by `id` first; each element is then asserted equal as a JSON value. No client can
//! depend on an order Go itself does not keep.
//!
//! # Why every fixture user is patched after it logs in
//!
//! The same cache is **stale after every login**: `DoLogin` → `UpdateLastLogin` writes
//! `Users.UpdateAt` (user_store.go:502) and nothing invalidates `userProfileByIdsCache`, so Go
//! answers the pre-login `update_at` — through this route and through `GET /users/{id}` — until
//! the entry is evicted. Measured: a fresh login, then Go said `…234499` and the row said
//! `…304155`. The port reads the row. [`refresh_go_user_cache`] is an empty `PATCH`, a write Go
//! *does* invalidate on (`UpdateUser`, app/user.go:1680), so the compared state is coherent. The
//! fixture admin, logged in once per binary and never patched, is never in a compared list.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, post_both_raw, purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/users/ids";

struct Fixture {
    admin_token: String,
    alive: Vec<common::PlainUser>,
    /// Created and then deactivated through Go's `DELETE /users/{id}` — a row with a
    /// non-zero `delete_at` that the query must still return.
    deactivated: common::PlainUser,
}

async fn fixture(client: &reqwest::Client, tag: &str) -> Fixture {
    purge_api_fixtures().await;
    let admin_token = go_minted_token(client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, &admin_token).await;

    let mut alive = Vec::new();
    for n in ["one", "two", "three"] {
        let user = create_plain_user(client, &admin_token, &team_id, &format!("{tag}{n}")).await;
        refresh_go_user_cache(client, &admin_token, &user.id).await;
        alive.push(user);
    }
    let deactivated =
        create_plain_user(client, &admin_token, &team_id, &format!("{tag}gone")).await;
    // Deactivation is `UpdateActive` → `UpdateUser`: it invalidates by itself.
    delete_plain_user(client, &admin_token, &deactivated.id).await;

    Fixture {
        admin_token,
        alive,
        deactivated,
    }
}

/// Make Go's profile cache agree with the database for `user_id`: an empty patch goes through
/// `UpdateUser`, which persists a fresh `UpdateAt` and invalidates the cache together.
async fn refresh_go_user_cache(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn teardown(client: &reqwest::Client, f: &Fixture) {
    for user in f.alive.iter().chain(std::iter::once(&f.deactivated)) {
        delete_plain_user(client, &f.admin_token, &user.id).await;
    }
}

/// Parse a list body and sort it by `id`, so two servers with different cache states can be
/// compared element by element.
fn sorted_by_id(body: &[u8], context: &str) -> Vec<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    });
    let mut list = parsed
        .as_array()
        .unwrap_or_else(|| panic!("{context}: not an array"))
        .clone();
    list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    list
}

/// POST the same body to both, assert 200 on both, and return the two id-sorted lists.
async fn both_lists(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &serde_json::Value,
    context: &str,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>, Vec<u8>) {
    let raw = serde_json::to_vec(body).expect("serialises");
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(client, token, path, &raw).await;
    assert_eq!(
        go_status,
        200,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        200,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs_body)
    );
    (
        sorted_by_id(&go_body, context),
        sorted_by_id(&rs_body, context),
        rs_body,
    )
}

/// The admin asks for three live users, the deactivated one, an unknown id and a duplicate —
/// out of order. Same set, same per-user fields, admin-level sanitisation (email present), the
/// deactivated user included, the unknown id absent, no trailing newline.
#[tokio::test]
async fn an_admin_gets_the_same_users_with_the_same_fields() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ubiadm").await;

    let body = serde_json::json!([
        f.alive[2].id,
        f.deactivated.id,
        "zzzzzzzzzzzzzzzzzzzzzzzzzz",
        f.alive[0].id,
        f.alive[1].id,
        f.alive[0].id,
    ]);
    let (go, rs, rs_raw) = both_lists(&client, &f.admin_token, PATH, &body, "admin list").await;
    teardown(&client, &f).await;

    assert_eq!(
        rs, go,
        "the two servers must return the same users with the same fields"
    );
    assert_eq!(
        rs.len(),
        4,
        "three live + deactivated; duplicate collapsed, unknown absent"
    );
    assert_ne!(rs_raw.last(), Some(&b'\n'), "Marshal appends nothing");

    let gone = rs
        .iter()
        .find(|u| u["id"] == f.deactivated.id.as_str())
        .expect("the deactivated user is returned — no DeleteAt filter");
    assert_ne!(gone["delete_at"], 0);

    let one = &rs[0];
    assert!(
        one["email"].as_str().is_some_and(|e| !e.is_empty()),
        "an admin sees email (SanitizeProfile with the admin override)"
    );
    assert!(one.get("password").is_none(), "never on the wire");
    assert!(
        one.get("last_password_update").is_none(),
        "ClearNonProfileFields blanks it for an admin viewer too"
    );
}

/// A plain user asks for the same list, including itself: the non-admin `SanitizeProfile`,
/// which strips what the privacy settings do not allow — and strips it from the caller's own
/// row too, where `GET /users/me` would not.
#[tokio::test]
async fn a_plain_user_gets_the_non_admin_sanitisation_including_for_itself() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ubipln").await;

    let body = serde_json::json!([f.alive[0].id, f.alive[1].id, f.alive[2].id]);
    let (go, rs, _) = both_lists(&client, &f.alive[0].token, PATH, &body, "plain list").await;

    // The same three through the admin, to prove the two sanitisations differ on the wire.
    let (admin_go, _, _) = both_lists(&client, &f.admin_token, PATH, &body, "admin again").await;
    teardown(&client, &f).await;

    assert_eq!(rs, go);
    assert_eq!(rs.len(), 3);
    let me = rs
        .iter()
        .find(|u| u["id"] == f.alive[0].id.as_str())
        .expect("the caller's own row");
    assert!(
        me.get("last_password_update").is_none()
            && me.get("notify_props").is_none()
            && me.get("auth_data").is_some(),
        "the caller's own row goes through the strict non-admin SanitizeProfile like every \
         other — no self exception on this route (contrast GET /users/me): {me}"
    );
    let plain_keys: std::collections::BTreeSet<&str> = rs[0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    let admin_keys: std::collections::BTreeSet<&str> = admin_go[0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_ne!(
        plain_keys, admin_keys,
        "the admin override must make an observable difference, or this test proves nothing \
         about IsSystemAdmin"
    );
    // Not "fewer keys": `ClearNonProfileFields` for a non-admin *adds* `auth_data` (it sets
    // the pointer to `""`, which omitempty keeps) while removing `notify_props`.
    assert!(
        plain_keys.contains("auth_data") && !admin_keys.contains("auth_data"),
        "plain {plain_keys:?} vs admin {admin_keys:?}"
    );
    assert!(
        admin_keys.contains("notify_props") && !plain_keys.contains("notify_props"),
        "plain {plain_keys:?} vs admin {admin_keys:?}"
    );
}

/// `since`: users created one after another have increasing `update_at`; asking with the
/// middle one's value drops it and everything older on both servers (strictly greater).
#[tokio::test]
async fn since_drops_users_not_updated_after_it() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ubisnc").await;

    let all = serde_json::json!([f.alive[0].id, f.alive[1].id, f.alive[2].id]);
    let (go_all, _, _) = both_lists(&client, &f.admin_token, PATH, &all, "baseline").await;
    let mut by_update: Vec<(i64, &str)> = go_all
        .iter()
        .map(|u| {
            (
                u["update_at"].as_i64().expect("i64"),
                u["id"].as_str().expect("id"),
            )
        })
        .collect();
    by_update.sort();
    assert!(
        by_update[0].0 < by_update[1].0 && by_update[1].0 < by_update[2].0,
        "the fixture needs three distinct update_at values: {by_update:?}"
    );
    let middle = by_update[1].0;

    let (go, rs, _) = both_lists(
        &client,
        &f.admin_token,
        &format!("{PATH}?since={middle}"),
        &all,
        "since=middle",
    )
    .await;

    // A negative `since` is legal and means no filter.
    let (go_neg, rs_neg, _) = both_lists(
        &client,
        &f.admin_token,
        &format!("{PATH}?since=-1"),
        &all,
        "since=-1",
    )
    .await;
    teardown(&client, &f).await;

    assert_eq!(rs, go);
    assert_eq!(rs.len(), 1, "only the newest survives `UpdateAt > middle`");
    assert_eq!(rs[0]["id"], by_update[2].1);

    assert_eq!(rs_neg, go_neg);
    assert_eq!(rs_neg.len(), 3, "a negative since filters nothing");
}

/// The refusals, each compared against Go's body; and the two non-refusals a reader would
/// expect to be refused: a too-short id (no length check here) and trailing bytes.
#[tokio::test]
async fn refusals_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (path, body, expected_id, expected_name, context) in [
        (
            PATH,
            &b"not json"[..],
            "api.payload.parse.error",
            None,
            "unparseable body",
        ),
        (
            PATH,
            &b"[1]"[..],
            "api.payload.parse.error",
            None,
            "a number",
        ),
        (
            PATH,
            &b"[]"[..],
            "api.context.invalid_body_param.app_error",
            Some("user_ids"),
            "empty array",
        ),
        (
            PATH,
            &b"null"[..],
            "api.context.invalid_body_param.app_error",
            Some("user_ids"),
            "null",
        ),
        (
            "/api/v4/users/ids?since=abc",
            &b"[\"zzzzzzzzzzzzzzzzzzzzzzzzzz\"]"[..],
            "api.context.invalid_body_param.app_error",
            Some("since"),
            "since is not an integer",
        ),
        (
            "/api/v4/users/ids?since=abc",
            &b"[]"[..],
            "api.context.invalid_body_param.app_error",
            Some("user_ids"),
            "the empty list is checked before since",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, path, body).await;
        assert_eq!(go_status, 400, "{context}: Go");
        assert_eq!(rs_status, 400, "{context}: Rust");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        assert_eq!(go["id"], expected_id, "{context}");
        if let Some(name) = expected_name {
            assert_eq!(go["detailed_error"], "", "{context}: developer mode is off");
            let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("json");
            assert_eq!(
                rs["message"]
                    .as_str()
                    .map(|m| m.contains("invalid_body_param")),
                Some(true)
            );
            // The parameter name is interpolated into Go's translated message only; pin ours
            // through the unit tests and here just that Go named the same branch.
            assert!(
                go["message"].as_str().is_some_and(|m| m.contains(name)),
                "{context}: Go's message names {name}: {}",
                go["message"]
            );
        }
    }

    // A two-character id is not a refusal on this route — there is no length check, unlike
    // `/users/status/ids` — and trailing bytes are never read.
    for (body, context) in [
        (&b"[\"zz\"]"[..], "short id"),
        (
            &b"[\"zzzzzzzzzzzzzzzzzzzzzzzzzz\"] trailing"[..],
            "trailing bytes",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, body).await;
        assert_eq!(go_status, 200, "{context}: Go");
        assert_eq!(rs_status, 200, "{context}: Rust");
        assert_eq!(rs_body, go_body, "{context}: both are an empty list");
        assert_eq!(rs_body, b"[]");
    }
}

/// `GET` on the POST-only literal keeps being forwarded, so Go's own answer stands.
#[tokio::test]
async fn get_on_the_ids_literal_is_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let rs = client
        .get(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go")
    );
    let go = client
        .get(format!("{GO}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(rs.status(), go.status());
}
