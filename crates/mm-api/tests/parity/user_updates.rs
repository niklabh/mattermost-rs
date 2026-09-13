//! Cross-server parity for the user-update family.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity user_updates
//! ```
//!
//! # Two accounts per comparison, because a write cannot be replayed
//!
//! `PUT /users/{id}` is idempotent in the value it sets but not in what it *reports*: `UpdateAt`
//! moves with the clock, and the second server would see the row the first already changed. So
//! every comparison mints a **pair** of accounts with identical profiles and different
//! identities, applies the same body to one on each base, and compares the two responses with
//! `id`, `username`, `email` and the timestamps removed — the same arrangement `user_creates`
//! uses, for the same reason.
//!
//! The **refusals** are exempt from that and are compared as whole bodies, which is where most of
//! these assertions live. A refusal has no identity in it.
//!
//! # Every account is hard-deleted by the test that made it
//!
//! Go's `DELETE /users/{id}` is a *soft* delete: the row stays and `users_stats` still counts it.
//! `scrub` removes the row. The prefix `mmrsupduser` is this file's alone, so an abandoned run
//! cannot be confused with `mmrsplain` or `mmrsnewuser`, and `common::purge_api_fixtures` sweeps
//! it at the start of the binary.
//!
//! # `common::USER_COUNT` is held by every test that creates or deactivates an account
//!
//! For the reason its doc comment gives: `users_stats` compares a `COUNT(*)` across two servers
//! and a create in flight is an off-by-one in a suite that has nothing to do with this one.
//! Deactivation counts too — `GET /users/stats` is `Count(UserCountOptions{})`, whose first
//! predicate is `DeleteAt = 0`.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, stack_enabled,
};

const PASSWORD: &str = "Mmrs-Upd-1234";

fn username(tag: &str) -> String {
    format!("mmrsupduser{tag}")
}

fn email(tag: &str) -> String {
    format!("{}@mmrs.invalid", username(tag))
}

async fn pool() -> Option<sqlx::PgPool> {
    common::fixture_pool().await
}

/// Remove every trace of an account this suite created — hard, not soft.
async fn scrub(tag: &str) {
    let Some(pool) = pool().await else {
        return;
    };
    let username = username(tag);
    for statement in [
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM teammembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM channelmembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM status WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM users WHERE username = $1",
    ] {
        let _ = sqlx::query(statement).bind(&username).execute(&pool).await;
    }
}

/// One stored column of one account, by username — every assertion about persisted state in this
/// file is scoped to a row this test created. Nothing here counts anything global.
async fn column_of(tag: &str, column: &str) -> Option<String> {
    let pool = pool().await?;
    // The column name is a literal from this file, never from a test input.
    let sql = format!("SELECT {column}::text FROM users WHERE username = $1");
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(username(tag))
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Create one account with a fully populated profile, through Go's admin API.
async fn make_user(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> String {
    scrub(tag).await;
    let response = http
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "email": email(tag),
            "username": username(tag),
            "password": PASSWORD,
            "nickname": "StartNick",
            "first_name": "StartFirst",
            "last_name": "StartLast",
            "position": "StartPosition",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating {} failed: {}",
        username(tag),
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    let id = created["id"].as_str().expect("an id").to_owned();

    let joined = http
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": id }))
        .send()
        .await
        .expect("Go answers");
    assert!(joined.status().is_success(), "team join failed");

    id
}

/// The pair of accounts a comparison needs: `<tag>go` and `<tag>rs`, identical but for identity.
async fn pair(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> (String, String) {
    let go = make_user(http, admin, team_id, &format!("{tag}go")).await;
    let rs = make_user(http, admin, team_id, &format!("{tag}rs")).await;
    (go, rs)
}

async fn scrub_pair(tag: &str) {
    scrub(&format!("{tag}go")).await;
    scrub(&format!("{tag}rs")).await;
}

/// Log one of this suite's accounts in, returning a fresh token.
///
/// Fresh matters: `SessionHasPermissionTo` reads the roles copied onto the **session** row at
/// login, so a token minted before a role change carries the old set.
async fn login(http: &reqwest::Client, tag: &str) -> String {
    let response = http
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username(tag), "password": PASSWORD }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "{} cannot log in", username(tag));
    response
        .headers()
        .get("token")
        .expect("a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

/// A `PUT` to one base. `expect_rust` asserts `x-mmrs-served-by: rust` — the check that caught a
/// whole parity suite passing against a stale proxy.
async fn put(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: serde_json::Value,
    expect_rust: bool,
) -> (u16, serde_json::Value) {
    let (status, bytes, _) = put_raw_with_header(
        http,
        base,
        path,
        token,
        &serde_json::to_vec(&body).expect("a body"),
        expect_rust,
    )
    .await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn put_raw(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &[u8],
    expect_rust: bool,
) -> (u16, Vec<u8>) {
    let (status, bytes, _) = put_raw_with_header(http, base, path, token, body, expect_rust).await;
    (status, bytes)
}

async fn put_raw_with_header(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &[u8],
    expect_rust: bool,
) -> (u16, Vec<u8>, Option<String>) {
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if base == RUST && expect_rust {
        common::assert_served_by_rust(response.headers(), path);
    }
    let bytes = response.bytes().await.expect("a body").to_vec();
    (status, bytes, served)
}

/// The fields two different accounts must still agree on after the same update.
fn comparable(user: &serde_json::Value) -> serde_json::Value {
    let mut map = user.as_object().cloned().unwrap_or_default();
    for volatile in [
        "id",
        "username",
        "email",
        "create_at",
        "update_at",
        "last_password_update",
    ] {
        map.remove(volatile);
    }
    serde_json::Value::Object(map)
}

/// A `model.User` body that only says what a caller would say.
fn body_for(id: &str, tag: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut map = serde_json::json!({
        "id": id,
        "username": username(tag),
        "email": email(tag),
    });
    if let (Some(map), Some(extra)) = (map.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

async fn admin_and_team(http: &reqwest::Client) -> (String, String) {
    let admin = go_minted_token(http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(http, &admin).await;
    (admin, team_id)
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}
// ---------------------------------------------------------------------------------------------

/// The ordinary update agrees field for field.
#[tokio::test]
async fn an_update_agrees_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "plain").await;

    let extra = serde_json::json!({
        "nickname": "EditedNick",
        "first_name": "EditedFirst",
        "last_name": "EditedLast",
        "position": "EditedPosition",
        "locale": "fr",
    });

    let (go_status, go_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}"),
        &admin,
        body_for(&go_id, "plaingo", extra.clone()),
        false,
    )
    .await;
    let (rs_status, rs_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}"),
        &admin,
        body_for(&rs_id, "plainrs", extra),
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go refused the update: {go_user}");
    assert_eq!(rs_status, 200, "we refused the update: {rs_user}");
    assert_eq!(
        comparable(&go_user),
        comparable(&rs_user),
        "the two updated users differ beyond their identities"
    );
    assert_eq!(rs_user["nickname"], "EditedNick");
    assert_eq!(rs_user["locale"], "fr");

    scrub_pair("plain").await;
}

/// **The privilege test.** A body claiming `roles`, `delete_at`, `email_verified` and the
/// timestamps changes none of them — on either server.
///
/// `SqlUserStore.Update` copies thirteen columns off the stored row before it builds the
/// statement, and `trustedUpdateData = false` adds `Roles` and `DeleteAt`. There is no
/// `SanitizeInput` on this route; the store is the whole protection. A port that trusted the body
/// would make this route a self-service `system_admin` grant, and the response body alone would
/// not say so — hence the `SELECT` at the end.
#[tokio::test]
async fn the_body_cannot_grant_itself_roles_or_undelete_itself() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "priv").await;

    let extra = serde_json::json!({
        "nickname": "PrivNick",
        "roles": "system_user system_admin",
        "delete_at": 1_600_000_000_000i64,
        "email_verified": true,
        "create_at": 1i64,
        "failed_attempts": 9,
        "last_password_update": 1i64,
        "mfa_active": true,
        "remote_id": "abcdefghijklmnopqrstuvwxyz",
        "auth_service": "gitlab",
    });

    let (go_status, go_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}"),
        &admin,
        body_for(&go_id, "privgo", extra.clone()),
        false,
    )
    .await;
    let (rs_status, rs_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}"),
        &admin,
        body_for(&rs_id, "privrs", extra),
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go refused: {go_user}");
    assert_eq!(rs_status, 200, "we refused: {rs_user}");
    assert_eq!(comparable(&go_user), comparable(&rs_user));

    // The response says so...
    assert_eq!(
        rs_user["roles"], "system_user",
        "the body did not grant roles"
    );
    assert_eq!(rs_user["delete_at"], 0);
    assert_eq!(
        rs_user["nickname"], "PrivNick",
        "and the profile field did change"
    );

    // ...and so does the row, which is the assertion that matters. Scoped to this test's own
    // account by username; nothing global is counted.
    assert_eq!(
        column_of("privrs", "roles").await.as_deref(),
        Some("system_user"),
        "the stored roles are untouched"
    );
    assert_eq!(column_of("privrs", "deleteat").await.as_deref(), Some("0"));
    assert_eq!(
        column_of("privrs", "authservice").await.as_deref(),
        Some(""),
        "auth_service is copied off the stored row"
    );
    // `remote_id` is in the same unconditional copy-back, and on **this** route it is the only
    // protection there is: `updateUser` has no `SanitizeInput` and, unlike `patchUser`, no
    // explicit nil-ing. A body claiming a remote id would otherwise mark the row as owned by
    // another cluster. Added after `api-patch-keeps-the-remote-id` survived — see the note in
    // `scripts/mutations/user-update.plan`.
    assert_ne!(
        column_of("privrs", "remoteid").await.as_deref(),
        Some("abcdefghijklmnopqrstuvwxyz"),
        "the body claimed a remote id"
    );
    assert_eq!(
        column_of("privrs", "remoteid").await,
        column_of("privgo", "remoteid").await,
        "and both servers stored the same thing"
    );
    assert_eq!(
        column_of("privrs", "emailverified").await.as_deref(),
        column_of("privgo", "emailverified").await.as_deref(),
        "and both servers agree about email_verified"
    );

    scrub_pair("priv").await;
}

/// A body whose `id` is not the path's id is a 400 naming `user_id`, on both servers — and the
/// row is untouched.
#[tokio::test]
async fn a_mismatched_body_id_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "mism").await;

    let other = "abcdefghijklmnopqrstuvwxyz";
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}"),
        &admin,
        serde_json::to_vec(&body_for(other, "mismgo", serde_json::json!({})))
            .expect("a body")
            .as_slice(),
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}"),
        &admin,
        serde_json::to_vec(&body_for(other, "mismrs", serde_json::json!({})))
            .expect("a body")
            .as_slice(),
        true,
    )
    .await;

    assert_eq!(go_status, 400, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 400, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a mismatched body id");

    assert_eq!(
        column_of("mismrs", "nickname").await.as_deref(),
        Some("StartNick"),
        "nothing was written"
    );

    scrub_pair("mism").await;
}

/// A plain user may not update somebody else: the same 403 from both servers.
#[tokio::test]
async fn a_stranger_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "strg").await;
    let actor = make_user(&http, &admin, &team, "strgactor").await;
    let _ = actor;
    let actor_token = login(&http, "strgactor").await;

    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}"),
        &actor_token,
        serde_json::to_vec(&body_for(&go_id, "strggo", serde_json::json!({})))
            .expect("a body")
            .as_slice(),
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}"),
        &actor_token,
        serde_json::to_vec(&body_for(&rs_id, "strgrs", serde_json::json!({})))
            .expect("a body")
            .as_slice(),
        true,
    )
    .await;

    assert_eq!(go_status, 403, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 403, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a stranger's update");

    scrub_pair("strg").await;
    scrub("strgactor").await;
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/patch
// ---------------------------------------------------------------------------------------------

/// The substance of the pair: **`patch` merges where `update` replaces.**
///
/// One body mentioning only the nickname, sent to `/patch` on both servers and to the plain
/// route on two more accounts. The patched users keep `position`; the updated users lose it,
/// because `updateUser` writes the whole struct and an absent `position` is `""`.
#[tokio::test]
async fn patch_merges_where_update_replaces() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (patch_go, patch_rs) = pair(&http, &admin, &team, "mrgp").await;
    let (put_go, put_rs) = pair(&http, &admin, &team, "mrgu").await;

    let patch_body = serde_json::json!({ "nickname": "MergedNick" });

    let (pg_status, pg_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{patch_go}/patch"),
        &admin,
        patch_body.clone(),
        false,
    )
    .await;
    let (pr_status, pr_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{patch_rs}/patch"),
        &admin,
        patch_body,
        true,
    )
    .await;

    assert_eq!(pg_status, 200, "Go refused the patch: {pg_user}");
    assert_eq!(pr_status, 200, "we refused the patch: {pr_user}");
    assert_eq!(comparable(&pg_user), comparable(&pr_user));

    // The merge: the fields the patch did not mention survive.
    assert_eq!(pr_user["nickname"], "MergedNick");
    assert_eq!(pr_user["position"], "StartPosition", "patch merges");
    assert_eq!(pr_user["first_name"], "StartFirst");

    // The same intent through `PUT /users/{id}` — a whole-struct write, so the unmentioned
    // fields are cleared. Both servers agree about that too.
    let (ug_status, ug_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{put_go}"),
        &admin,
        body_for(
            &put_go,
            "mrgugo",
            serde_json::json!({ "nickname": "MergedNick" }),
        ),
        false,
    )
    .await;
    let (ur_status, ur_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{put_rs}"),
        &admin,
        body_for(
            &put_rs,
            "mrgurs",
            serde_json::json!({ "nickname": "MergedNick" }),
        ),
        true,
    )
    .await;

    assert_eq!(ug_status, 200, "Go refused the update: {ug_user}");
    assert_eq!(ur_status, 200, "we refused the update: {ur_user}");
    assert_eq!(comparable(&ug_user), comparable(&ur_user));
    assert_eq!(ur_user["position"], "", "update replaces");
    assert_eq!(ur_user["first_name"], "");

    scrub_pair("mrgp").await;
    scrub_pair("mrgu").await;
}

/// An explicit `null` in a patch is indistinguishable from an absent key: neither clears the
/// field, while `""` does. Both servers.
#[tokio::test]
async fn a_null_in_a_patch_is_not_a_clear() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "null").await;

    let body = serde_json::json!({ "nickname": null, "position": "" });
    let (go_status, go_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/patch"),
        &admin,
        body.clone(),
        false,
    )
    .await;
    let (rs_status, rs_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/patch"),
        &admin,
        body,
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go: {go_user}");
    assert_eq!(rs_status, 200, "us: {rs_user}");
    assert_eq!(comparable(&go_user), comparable(&rs_user));
    assert_eq!(rs_user["nickname"], "StartNick", "null did not clear it");
    assert_eq!(rs_user["position"], "", "the empty string did");

    scrub_pair("null").await;
}

/// `patch.RemoteId = nil` — a patch claiming a remote id is accepted and the field is discarded.
#[tokio::test]
async fn a_patch_cannot_claim_a_remote_id() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "rmid").await;

    let body = serde_json::json!({
        "nickname": "RemoteNick",
        "remote_id": "abcdefghijklmnopqrstuvwxyz",
    });
    let (go_status, go_user) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/patch"),
        &admin,
        body.clone(),
        false,
    )
    .await;
    let (rs_status, rs_user) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/patch"),
        &admin,
        body,
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go: {go_user}");
    assert_eq!(rs_status, 200, "us: {rs_user}");
    assert_eq!(comparable(&go_user), comparable(&rs_user));
    assert_eq!(rs_user["nickname"], "RemoteNick");
    let stored = column_of("rmidrs", "remoteid").await;
    assert!(
        stored.as_deref().unwrap_or("") != "abcdefghijklmnopqrstuvwxyz",
        "the remote id was written: {stored:?}"
    );

    scrub_pair("rmid").await;
}

/// The two routes answer an **unknown but well-formed** user id differently, and both servers
/// agree about the difference: `/patch` is a 400 naming a body parameter, the plain route a 404.
#[tokio::test]
async fn an_unknown_id_is_a_400_on_patch_and_a_404_on_update() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let missing = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    let (go_patch_status, go_patch_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{missing}/patch"),
        &admin,
        br#"{"nickname":"x"}"#,
        false,
    )
    .await;
    let (rs_patch_status, rs_patch_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{missing}/patch"),
        &admin,
        br#"{"nickname":"x"}"#,
        true,
    )
    .await;
    assert_eq!(
        go_patch_status,
        400,
        "Go: {}",
        String::from_utf8_lossy(&go_patch_body)
    );
    assert_eq!(
        rs_patch_status,
        400,
        "us: {}",
        String::from_utf8_lossy(&rs_patch_body)
    );
    assert_error_bodies_match_except_known_gaps(
        &go_patch_body,
        &rs_patch_body,
        "patch, unknown id",
    );

    let update_body =
        serde_json::to_vec(&serde_json::json!({ "id": missing, "nickname": "x" })).expect("a body");
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{missing}"),
        &admin,
        &update_body,
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{missing}"),
        &admin,
        &update_body,
        true,
    )
    .await;
    assert_eq!(go_status, 404, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 404, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "update, unknown id");
}

/// Changing **your own** e-mail needs the current password, and the two routes disagree about the
/// failure: `/patch` propagates the credential error, the plain route flattens every failure to a
/// 400 naming `password`.
#[tokio::test]
async fn changing_your_own_email_needs_the_current_password() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "mail").await;
    let go_token = login(&http, "mailgo").await;
    let rs_token = login(&http, "mailrs").await;

    // `/patch` with no password at all: a 400 naming `password` on both.
    let body = serde_json::json!({ "email": "mmrsupdmoved@mmrs.invalid" });
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/patch"),
        &go_token,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/patch"),
        &rs_token,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        true,
    )
    .await;
    assert_eq!(go_status, 400, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 400, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "patch, no password");

    // The plain route with an empty password: also a 400 naming `password`, but by a different
    // route through the code — `DoubleCheckPassword("")` fails and every failure is flattened.
    let go_update = serde_json::to_vec(&body_for(
        &go_id,
        "mailgo",
        serde_json::json!({ "email": "mmrsupdmoved2@mmrs.invalid" }),
    ))
    .expect("a body");
    let rs_update = serde_json::to_vec(&body_for(
        &rs_id,
        "mailrs",
        serde_json::json!({ "email": "mmrsupdmoved2@mmrs.invalid" }),
    ))
    .expect("a body");
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}"),
        &go_token,
        &go_update,
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}"),
        &rs_token,
        &rs_update,
        true,
    )
    .await;
    assert_eq!(go_status, 400, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 400, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "update, empty password");

    // Neither attempt moved the address.
    assert_eq!(column_of("mailrs", "email").await, Some(email("mailrs")));
    assert_eq!(column_of("mailgo", "email").await, Some(email("mailgo")));

    scrub_pair("mail").await;
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/roles
// ---------------------------------------------------------------------------------------------

/// A role change agrees, and it reaches the `Sessions` rows as well as the `Users` row.
#[tokio::test]
async fn a_role_change_agrees_and_reaches_the_sessions() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "role").await;
    // A live session apiece, so the `Sessions.Roles` write has something to land on.
    let _ = login(&http, "rolego").await;
    let _ = login(&http, "rolers").await;

    let body = serde_json::json!({ "roles": "system_user system_post_all" });
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/roles"),
        &admin,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/roles"),
        &admin,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_eq!(go_body, rs_body, "both answer ReturnStatusOK byte for byte");
    assert_eq!(
        rs_body, br#"{"status":"OK"}"#,
        "and it has no trailing newline"
    );

    assert_eq!(
        column_of("rolers", "roles").await.as_deref(),
        Some("system_user system_post_all")
    );
    assert_eq!(
        session_roles("rolers").await.as_deref(),
        Some("system_user system_post_all"),
        "the session row was updated too"
    );
    assert_eq!(session_roles("rolego").await, session_roles("rolers").await);

    scrub_pair("role").await;
}

/// The roles on this account's session rows, scoped by username.
async fn session_roles(tag: &str) -> Option<String> {
    let pool = pool().await?;
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT roles FROM sessions
          WHERE userid IN (SELECT id FROM users WHERE username = $1)
          ORDER BY createat DESC LIMIT 1",
    )
    .bind(username(tag))
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten()
    .flatten()
}

/// The three refusals of `updateUserRoles`, compared as whole bodies.
///
/// `"system_admin"` **alone** is invalid — Go excludes it explicitly "to prevent mistakes" — while
/// `"system_user system_admin"` is fine. A licence-gated role is a 400 on this unlicensed stack,
/// and it is raised *before* the permission check, so an unprivileged caller sees it too.
#[tokio::test]
async fn the_role_refusals_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "rolr").await;

    for (what, body) in [
        ("system_admin alone", &br#"{"roles":"system_admin"}"#[..]),
        ("a bad role name", &br#"{"roles":"NOT-A-ROLE"}"#[..]),
        (
            "no such role",
            &br#"{"roles":"system_user mmrs_missing_role"}"#[..],
        ),
        (
            "a licence-gated role",
            &br#"{"roles":"system_user system_manager"}"#[..],
        ),
    ] {
        let (go_status, go_body) = put_raw(
            &http,
            GO,
            &format!("/api/v4/users/{go_id}/roles"),
            &admin,
            body,
            false,
        )
        .await;
        let (rs_status, rs_body) = put_raw(
            &http,
            RUST,
            &format!("/api/v4/users/{rs_id}/roles"),
            &admin,
            body,
            true,
        )
        .await;
        assert_eq!(
            go_status,
            rs_status,
            "{what}: Go {go_status} vs us {rs_status} — {} / {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(go_status, 400, "{what} should be a 400");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);

        assert_eq!(
            column_of("rolrrs", "roles").await.as_deref(),
            Some("system_user"),
            "{what} wrote nothing"
        );
    }

    scrub_pair("rolr").await;
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/active
// ---------------------------------------------------------------------------------------------

/// Reactivation agrees, and the row comes back.
#[tokio::test]
async fn reactivation_agrees_and_clears_delete_at() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "actv").await;

    // Deactivate both through Go, so the reactivation has something to undo.
    for id in [&go_id, &rs_id] {
        let response = http
            .put(format!("{GO}/api/v4/users/{id}/active"))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&serde_json::json!({ "active": false }))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(response.status(), 200, "the deactivation failed");
    }
    assert_ne!(column_of("actvrs", "deleteat").await.as_deref(), Some("0"));

    let body = serde_json::json!({ "active": true });
    let (go_status, go_body) = put_raw(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/active"),
        &admin,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        false,
    )
    .await;
    let (rs_status, rs_body) = put_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/active"),
        &admin,
        serde_json::to_vec(&body).expect("a body").as_slice(),
        true,
    )
    .await;

    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_eq!(go_body, rs_body);
    assert_eq!(rs_body, br#"{"status":"OK"}"#);

    assert_eq!(column_of("actvrs", "deleteat").await.as_deref(), Some("0"));
    assert_eq!(column_of("actvgo", "deleteat").await.as_deref(), Some("0"));

    scrub_pair("actv").await;
}

/// `props["active"].(bool)` is a **type assertion**: a string, a number and a missing key are all
/// 400s naming `active`, on both servers, and none of them writes.
#[tokio::test]
async fn a_non_boolean_active_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "actb").await;

    for (what, body) in [
        ("a string", &br#"{"active":"true"}"#[..]),
        ("a number", &br#"{"active":1}"#[..]),
        ("a null", &br#"{"active":null}"#[..]),
        ("a missing key", &br#"{}"#[..]),
        ("a capitalised key", &br#"{"Active":true}"#[..]),
        ("an array", &br#"[]"#[..]),
    ] {
        let (go_status, go_body) = put_raw(
            &http,
            GO,
            &format!("/api/v4/users/{go_id}/active"),
            &admin,
            body,
            false,
        )
        .await;
        let (rs_status, rs_body) = put_raw(
            &http,
            RUST,
            &format!("/api/v4/users/{rs_id}/active"),
            &admin,
            body,
            true,
        )
        .await;
        assert_eq!(
            go_status,
            400,
            "{what}: Go {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            400,
            "{what}: us {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);
        assert_eq!(
            column_of("actbrs", "deleteat").await.as_deref(),
            Some("0"),
            "{what} wrote nothing"
        );
    }

    scrub_pair("actb").await;
}

/// **Deactivation forwards, and the forward happens before anything is written.**
///
/// `active = false` continues into `RevokeAllSessions` and `userDeactivated`, neither of which is
/// ported — see [D-461]. The test asserts both halves: the response carries no
/// `x-mmrs-served-by: rust`, and Go's own work (the row, and the revoked sessions) is done, which
/// is only possible if the request reached Go intact rather than after a partial local write.
#[tokio::test]
async fn deactivation_forwards_before_any_write() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let id = make_user(&http, &admin, &team, "deac").await;
    let _ = login(&http, "deac").await;
    assert!(
        session_roles("deac").await.is_some(),
        "the fixture has a live session to revoke"
    );

    let (status, body, served) = put_raw_with_header(
        &http,
        RUST,
        &format!("/api/v4/users/{id}/active"),
        &admin,
        br#"{"active":false}"#,
        false,
    )
    .await;

    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_ne!(
        served.as_deref(),
        Some("rust"),
        "deactivation must be forwarded, not served here"
    );
    assert_ne!(
        column_of("deac", "deleteat").await.as_deref(),
        Some("0"),
        "Go did the deactivation"
    );
    assert_eq!(
        session_roles("deac").await,
        None,
        "and RevokeAllSessions ran, which only Go can do"
    );

    scrub("deac").await;
}

// ---------------------------------------------------------------------------------------------
// The auto-responder forward
// ---------------------------------------------------------------------------------------------

/// **A patch that switches the auto-responder on forwards; one that switches it off does not.**
///
/// `SetAutoResponderStatus` runs *after* `PatchUser` writes, and its off→on arm calls
/// `SetStatusOutOfOffice`, which is not ported. The transition is therefore computed from the
/// stored props and the patch *before* the write, and the whole request is handed to Go. The
/// on→off arm is `SetStatusOnline(id, true)`, which is ported, so that direction is served.
///
/// Both halves are asserted, because a port that forwarded *every* patch carrying `notify_props`
/// would pass the first assertion and fail nothing else in this file.
#[tokio::test]
async fn switching_the_auto_responder_on_forwards_and_switching_it_off_does_not() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let id = make_user(&http, &admin, &team, "auto").await;

    let props = |active: &str| {
        serde_json::json!({
            "notify_props": {
                "auto_responder_active": active,
                "auto_responder_message": "away",
                "email": "true",
                "push": "mention",
                "desktop": "mention",
            }
        })
    };

    // off -> on: forwarded.
    let (status, body, served) = put_raw_with_header(
        &http,
        RUST,
        &format!("/api/v4/users/{id}/patch"),
        &admin,
        serde_json::to_vec(&props("true"))
            .expect("a body")
            .as_slice(),
        false,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_ne!(
        served.as_deref(),
        Some("rust"),
        "the off->on transition must be forwarded"
    );

    // on -> off: served here, because `SetStatusOnline` is ported.
    let (status, body, served) = put_raw_with_header(
        &http,
        RUST,
        &format!("/api/v4/users/{id}/patch"),
        &admin,
        serde_json::to_vec(&props("false"))
            .expect("a body")
            .as_slice(),
        false,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the on->off transition is ours: {}",
        String::from_utf8_lossy(&body)
    );

    // And a patch that touches no notify props at all is served, whatever the stored flag says.
    let (status, body, served) = put_raw_with_header(
        &http,
        RUST,
        &format!("/api/v4/users/{id}/patch"),
        &admin,
        br#"{"nickname":"AutoNick"}"#,
        false,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "a patch with no notify props is not an auto-responder change"
    );

    scrub("auto").await;
}
