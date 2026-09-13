//! Cross-server parity for the four authentication-data routes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity user_auth
//! ```
//!
//! ```text
//! PUT  /api/v4/users/{user_id}/auth
//! PUT  /api/v4/users/{user_id}/mfa
//! POST /api/v4/users/{user_id}/mfa/generate
//! POST /api/v4/users/login/switch
//! ```
//!
//! # These routes destroy credentials, so they only ever touch this file's own accounts
//!
//! `PUT /users/{id}/auth` blanks `Password` and revokes every session of its target. Every
//! subject here is minted under the prefix `mmrsauthuser`, which belongs to this file alone and
//! is swept by `common::purge_api_fixtures`. **Nothing here is pointed at the fixture
//! administrator the whole binary logs in as** — one call would end the run.
//!
//! `common::USER_COUNT` is held by every test that mints an account, for the same reason
//! `user_deletes` holds it: a creation moves `users_stats`.
//!
//! # What is asserted about the two MFA routes is that they refuse
//!
//! `EnableMultifactorAuthentication` is off stack-wide and cannot be turned on through the API,
//! so no test here activates MFA or generates a secret; the flag forward is asserted by reading
//! the configuration rather than by flipping it ([D-500]). What *is* asserted is the eight
//! refusals and that the one 200 — a deactivation — carries Go's answer rather than this
//! server's.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    go_minted_token, stack_enabled,
};

const PASSWORD: &str = "Mmrs-Auth-1234";

fn username(tag: &str) -> String {
    format!("mmrsauthuser{tag}")
}

fn email(tag: &str) -> String {
    format!("{}@mmrs.invalid", username(tag))
}

async fn pool() -> Option<sqlx::PgPool> {
    common::fixture_pool().await
}

/// Remove every trace of an account this suite created.
async fn scrub(tag: &str) {
    let Some(pool) = pool().await else {
        return;
    };
    let username = username(tag);
    for statement in [
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM teammembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM channelmembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM status WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM users WHERE username = $1",
    ] {
        let _ = sqlx::query(statement).bind(&username).execute(&pool).await;
    }
}

async fn scrub_pair(tag: &str) {
    scrub(&format!("{tag}go")).await;
    scrub(&format!("{tag}rs")).await;
}

/// One stored column of one of this suite's accounts, by username.
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

async fn number_of(tag: &str, column: &str) -> i64 {
    column_of(tag, column)
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or_default()
}

async fn session_count(tag: &str) -> i64 {
    let Some(pool) = pool().await else {
        return 0;
    };
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions
          WHERE userid IN (SELECT id FROM users WHERE username = $1)",
    )
    .bind(username(tag))
    .fetch_one(&pool)
    .await
    .unwrap_or_default()
}

/// Create one account through Go's admin API and put it on a team.
async fn make_user(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> String {
    common::purge_api_fixtures().await;
    scrub(tag).await;
    let response = http
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "email": email(tag),
            "username": username(tag),
            "password": PASSWORD,
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
    assert!(
        joined.status().is_success(),
        "team join failed: {}",
        joined.text().await.unwrap_or_default()
    );

    id
}

/// The pair of subjects a comparison needs: `<tag>go` and `<tag>rs`, identical but for identity.
async fn pair(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> (String, String) {
    let go = make_user(http, admin, team_id, &format!("{tag}go")).await;
    let rs = make_user(http, admin, team_id, &format!("{tag}rs")).await;
    (go, rs)
}

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

/// Whether a login with the account's original password still succeeds.
async fn can_still_log_in(http: &reqwest::Client, tag: &str) -> bool {
    let response = http
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username(tag), "password": PASSWORD }))
        .send()
        .await
        .expect("Go answers");
    response.status() == 200
}

/// A request against one base, returning the status, the raw body and `x-mmrs-served-by`.
async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.expect("a body").to_vec();
    (status, bytes, served)
}

async fn put(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    send(http, reqwest::Method::PUT, base, path, Some(token), body).await
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    send(http, reqwest::Method::POST, base, path, token, body).await
}

/// Send the same refusal to both servers and assert they agree on status, key set and every field
/// but `message` and `request_id`, and that this server answered it rather than forwarding.
async fn both_refuse(
    http: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: &str,
    expected_status: u16,
    expected_id: &str,
) {
    let (go_status, go_body, _) = send(http, method.clone(), GO, path, token, body).await;
    let (rs_status, rs_body, served) = send(http, method, RUST, path, token, body).await;
    let context = format!("{path} {body}");
    assert_eq!(
        go_status,
        expected_status,
        "{context}: Go answered {go_status}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{context}: {}",
        String::from_utf8_lossy(&rs_body)
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
    assert_eq!(go["id"].as_str(), Some(expected_id), "{context}");
    assert_eq!(served.as_deref(), Some("rust"), "{context}: served here");
}

async fn admin_and_team(http: &reqwest::Client) -> (String, String) {
    let admin = go_minted_token(http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(http, &admin).await;
    (admin, team_id)
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/auth
// ---------------------------------------------------------------------------------------------

/// **The `manage_system` gate runs before the id check, and only on this route.**
///
/// A caller without `manage_system` naming a malformed id gets the **403**; the same caller on
/// `POST /users/notanid/mfa/generate` gets the **400**, because that handler runs `RequireUserId`
/// first. Both halves are asserted together so that swapping the two checks in either handler
/// fails here — one alone would pass with the order reversed in the other.
#[tokio::test]
async fn the_auth_gate_precedes_the_id_check_and_the_mfa_gate_does_not() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    make_user(&http, &admin, &team, "gateorder").await;
    let plain = login(&http, "gateorder").await;

    both_refuse(
        &http,
        reqwest::Method::PUT,
        "/api/v4/users/notanid/auth",
        Some(&plain),
        r#"{"auth_service":"ldap","auth_data":"x"}"#,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/notanid/mfa/generate",
        Some(&plain),
        "",
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;

    // And the administrator, for whom the first gate passes, reaches the id check on `/auth`.
    both_refuse(
        &http,
        reqwest::Method::PUT,
        "/api/v4/users/notanid/auth",
        Some(&admin),
        r#"{"auth_service":"ldap","auth_data":"x"}"#,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;

    scrub("gateorder").await;
}

/// The four ways a body is refused, and they are **two different ids**: a body that will not
/// decode is `api.context.invalid_body_param.app_error` naming `user`, and a body that decodes
/// but fails `UserAuth.IsValid` is `api.user.update_user_auth.invalid_request`.
///
/// The `IsValid` cases are the whole security surface of the route's input: an unknown service,
/// a known service with no auth data, and `email` *with* auth data are all refused, so the route
/// cannot write an `AuthService` the login path does not understand or an SSO row with no
/// identity on it.
#[tokio::test]
async fn the_auth_body_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let id = make_user(&http, &admin, &team, "badbody").await;
    let path = format!("/api/v4/users/{id}/auth");

    both_refuse(
        &http,
        reqwest::Method::PUT,
        &path,
        Some(&admin),
        "notjson",
        400,
        "api.context.invalid_body_param.app_error",
    )
    .await;

    for body in [
        // No service at all.
        "{}",
        // A service nothing can authenticate against.
        r#"{"auth_service":"bogus","auth_data":"x"}"#,
        // A real service with no identity to match on.
        r#"{"auth_service":"gitlab"}"#,
        r#"{"auth_service":"ldap","auth_data":""}"#,
        // `email` must have **no** auth data.
        r#"{"auth_service":"email","auth_data":"x"}"#,
    ] {
        both_refuse(
            &http,
            reqwest::Method::PUT,
            &path,
            Some(&admin),
            body,
            400,
            "api.user.update_user_auth.invalid_request",
        )
        .await;
    }

    // Nothing above reached the store.
    assert_eq!(
        column_of("badbody", "authservice").await.as_deref(),
        Some(""),
        "a refused body changes nothing"
    );
    assert!(
        can_still_log_in(&http, "badbody").await,
        "a refused body leaves the password alone"
    );

    scrub("badbody").await;
}

/// **The accepted case, and everything it does beyond the two columns it is named for.**
///
/// The response is the submitted `UserAuth` echoed back — not a re-read — and the row afterwards
/// has a blanked `Password`, a zeroed `FailedAttempts`, `LastPasswordUpdate == UpdateAt` from one
/// clock read, and no sessions at all. The account can no longer log in with the password it had
/// a moment earlier, which is the point of the route and the thing a port that only wrote the two
/// auth columns would silently get wrong.
#[tokio::test]
async fn switching_an_account_to_sso_echoes_the_body_and_takes_the_password_with_it() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "sso").await;

    // A live session each, so the revocation has something to revoke, and a failed login each so
    // `FailedAttempts` is non-zero and the zeroing is visible.
    let _ = login(&http, "ssogo").await;
    let _ = login(&http, "ssors").await;
    for tag in ["ssogo", "ssors"] {
        let _ = http
            .post(format!("{GO}/api/v4/users/login"))
            .json(&serde_json::json!({ "login_id": username(tag), "password": "wrong" }))
            .send()
            .await;
        assert_eq!(
            number_of(tag, "failedattempts").await,
            1,
            "{tag}: there is a failed attempt to clear"
        );
        assert_eq!(session_count(tag).await, 1, "{tag}: a session to revoke");
    }

    // Distinct auth data per side: the column is unique, so one value for both would make the
    // second request a 400 and the test would be asserting the collision instead.
    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/auth"),
        &admin,
        r#"{"auth_service":"gitlab","auth_data":"mmrsauth-gitlab-go"}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/auth"),
        &admin,
        r#"{"auth_service":"gitlab","auth_data":"mmrsauth-gitlab-rs"}"#,
    )
    .await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"), "served here");
    assert_eq!(
        rs_body,
        br#"{"auth_data":"mmrsauth-gitlab-rs","auth_service":"gitlab"}"#
            .iter()
            .copied()
            .chain(std::iter::once(b'\n'))
            .collect::<Vec<u8>>(),
        "the encoder's trailing newline is on the wire"
    );
    assert_eq!(
        String::from_utf8_lossy(&go_body).replace("-go", "-X"),
        String::from_utf8_lossy(&rs_body).replace("-rs", "-X"),
        "the two bodies differ only in the value each was handed"
    );

    for (tag, data) in [
        ("ssogo", "mmrsauth-gitlab-go"),
        ("ssors", "mmrsauth-gitlab-rs"),
    ] {
        assert_eq!(
            column_of(tag, "authservice").await.as_deref(),
            Some("gitlab"),
            "{tag}: the service is stored"
        );
        assert_eq!(
            column_of(tag, "authdata").await.as_deref(),
            Some(data),
            "{tag}: the auth data is stored"
        );
        assert_eq!(
            column_of(tag, "password").await.as_deref(),
            Some(""),
            "{tag}: the password is blanked"
        );
        assert_eq!(
            number_of(tag, "failedattempts").await,
            0,
            "{tag}: the failed-attempt counter is cleared"
        );
        assert_eq!(
            number_of(tag, "updateat").await,
            number_of(tag, "lastpasswordupdate").await,
            "{tag}: one GetMillis() feeds both columns"
        );
        assert_eq!(session_count(tag).await, 0, "{tag}: every session is gone");
        assert!(
            !can_still_log_in(&http, tag).await,
            "{tag}: the old password no longer works"
        );
    }

    scrub_pair("sso").await;
}

/// **`{"auth_service":"email"}` answers two bytes**, because the handler blanks the service and
/// both fields carry `omitempty` — and it stores `AuthService = ''` with `AuthData` NULL, which
/// is what a password account looks like. The password is still blanked, so this is not an undo.
#[tokio::test]
async fn switching_back_to_email_answers_an_empty_object() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "toemail").await;

    for (base, id) in [(GO, &go_id), (RUST, &rs_id)] {
        let (status, _, _) = put(
            &http,
            base,
            &format!("/api/v4/users/{id}/auth"),
            &admin,
            r#"{"auth_service":"saml","auth_data":"mmrsauth-saml-x"}"#,
        )
        .await;
        assert_eq!(status, 200, "the setup switch must succeed");
        // The unique constraint on `AuthData` means the two sides cannot share a value; undo the
        // first before the second.
        let (status, _, _) = put(
            &http,
            base,
            &format!("/api/v4/users/{id}/auth"),
            &admin,
            r#"{"auth_service":"email"}"#,
        )
        .await;
        assert_eq!(status, 200);
    }

    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/auth"),
        &admin,
        r#"{"auth_service":"email"}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/auth"),
        &admin,
        r#"{"auth_service":"email"}"#,
    )
    .await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(rs_body, go_body, "identical bytes");
    assert_eq!(
        rs_body, b"{}\n",
        "an empty object and the encoder's newline"
    );
    assert_eq!(served.as_deref(), Some("rust"));

    for tag in ["toemailgo", "toemailrs"] {
        assert_eq!(
            column_of(tag, "authservice").await.as_deref(),
            Some(""),
            "{tag}: the service is blanked, not stored as `email`"
        );
        assert_eq!(
            column_of(tag, "authdata").await,
            None,
            "{tag}: the auth data is NULL"
        );
    }

    scrub_pair("toemail").await;
}

/// **An id that matches no row is a 200 that writes nothing.**
///
/// `UpdateAuthData` has no existence check and Go discards the row count, so the handler answers
/// with the `UserAuth` it was handed. A port that added the obvious `GetUser` would turn this
/// into a 404 — which is why the assertion is here rather than in a comment.
#[tokio::test]
async fn an_id_that_matches_nothing_is_a_two_hundred() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let (admin, _) = admin_and_team(&http).await;
    // 26 characters, valid shape, no such row.
    let path = "/api/v4/users/mmrsauthnosuchuseraaaaaaaa/auth";

    let (go_status, go_body, _) = put(
        &http,
        GO,
        path,
        &admin,
        r#"{"auth_service":"ldap","auth_data":"mmrsauth-ghost-go"}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        path,
        &admin,
        r#"{"auth_service":"ldap","auth_data":"mmrsauth-ghost-rs"}"#,
    )
    .await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        rs_body,
        b"{\"auth_data\":\"mmrsauth-ghost-rs\",\"auth_service\":\"ldap\"}\n"
    );

    if let Some(pool) = pool().await {
        let planted = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM users WHERE authdata LIKE 'mmrsauth-ghost-%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or_default();
        assert_eq!(planted, 0, "neither server created a row");
    }
}

/// **A duplicate `AuthData` is a 400 whose id talks about e-mail.**
///
/// `Users.AuthData` is unique, so moving a second account onto a value another account already
/// has raises Go's `ErrInvalidInput`, which the app layer renders as
/// `app.user.update_auth_data.email_exists.app_error`. The message is about an address; the
/// collision is on the identity. Both servers say the same wrong thing, which is the requirement.
#[tokio::test]
async fn a_duplicate_auth_data_is_the_email_exists_four_hundred() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let holder = make_user(&http, &admin, &team, "dupholder").await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "dup").await;

    let (status, body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{holder}/auth"),
        &admin,
        r#"{"auth_service":"ldap","auth_data":"mmrsauth-taken"}"#,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

    let taken = r#"{"auth_service":"ldap","auth_data":"mmrsauth-taken"}"#;
    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/auth"),
        &admin,
        taken,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/auth"),
        &admin,
        taken,
    )
    .await;

    assert_eq!(go_status, 400, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "duplicate auth data");
    assert_eq!(
        go["id"].as_str(),
        Some("app.user.update_auth_data.email_exists.app_error")
    );
    assert_eq!(served.as_deref(), Some("rust"));

    // The failed statement is one `UPDATE`, so nothing was applied — including the password blank.
    for tag in ["dupgo", "duprs"] {
        assert_eq!(
            column_of(tag, "authservice").await.as_deref(),
            Some(""),
            "{tag}: the refused switch left the row alone"
        );
        assert!(
            can_still_log_in(&http, tag).await,
            "{tag}: and left the password alone"
        );
    }

    scrub_pair("dup").await;
    scrub("dupholder").await;
}

/// **The body cannot smuggle a role, an address or a password.**
///
/// `model.UserAuth` has two fields; `encoding/json` discards every other key and the handler
/// never looks at the raw body again. A route that changes how an account authenticates and
/// trusted the body for anything else would be a privilege escalation — so the request that would
/// be one is sent, and the stored row is read back.
#[tokio::test]
async fn the_body_cannot_smuggle_a_role_or_an_address() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "smuggle").await;

    let body = |data: &str| {
        format!(
            r#"{{"auth_service":"ldap","auth_data":"{data}","roles":"system_admin",
                 "email":"mmrsauth-hijack@mmrs.invalid","password":"Hijacked-1234",
                 "email_verified":true,"mfa_active":true,"delete_at":1,"id":"nonsense"}}"#
        )
    };

    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/auth"),
        &admin,
        &body("mmrsauth-smuggle-go"),
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/auth"),
        &admin,
        &body("mmrsauth-smuggle-rs"),
    )
    .await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        rs_body, b"{\"auth_data\":\"mmrsauth-smuggle-rs\",\"auth_service\":\"ldap\"}\n",
        "the response carries the two fields and nothing the body added"
    );

    for tag in ["smugglego", "smugglers"] {
        assert_eq!(
            column_of(tag, "roles").await.as_deref(),
            Some("system_user"),
            "{tag}: the role in the body was ignored"
        );
        assert_eq!(
            column_of(tag, "email").await.as_deref(),
            Some(email(tag).as_str()),
            "{tag}: the address in the body was ignored"
        );
        assert_eq!(
            number_of(tag, "deleteat").await,
            0,
            "{tag}: the delete_at in the body was ignored"
        );
        assert_eq!(
            column_of(tag, "mfaactive").await.as_deref(),
            Some("false"),
            "{tag}: the mfa_active in the body was ignored"
        );
        // The password *is* changed — to nothing. What the body asked for is not what happened.
        assert_eq!(column_of(tag, "password").await.as_deref(), Some(""));
    }

    scrub_pair("smuggle").await;
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/users/{user_id}/mfa  and  POST /api/v4/users/{user_id}/mfa/generate
// ---------------------------------------------------------------------------------------------

/// **Every refusal on the MFA pair, in the order that makes each one reachable.**
///
/// The two body-parameter 400s come *before* `GetUser`, so an unknown id with a bad body is the
/// 400 and not the 404. The auth-service refusal comes before the disabled-MFA 501, so a `gitlab`
/// account is told it cannot use MFA at all rather than that the server has it off. Both orders
/// are asserted here, because reading them off the source in the wrong order is the whole failure
/// mode.
#[tokio::test]
async fn the_mfa_refusals_agree_and_keep_their_order() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let subject = make_user(&http, &admin, &team, "mfagates").await;
    let ghost = "mmrsauthnosuchuseraaaaaaaa";

    // `activate` must be a JSON boolean — a type assertion, not a parse.
    for body in [
        "{}",
        r#"{"activate":"true"}"#,
        r#"{"activate":1}"#,
        r#"{"activate":null}"#,
        "notjson",
    ] {
        both_refuse(
            &http,
            reqwest::Method::PUT,
            &format!("/api/v4/users/{subject}/mfa"),
            Some(&admin),
            body,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
    }

    // `code` must be a non-empty string, and this is checked before `GetUser` — so the *ghost*
    // gets the 400 too.
    for path in [
        format!("/api/v4/users/{subject}/mfa"),
        format!("/api/v4/users/{ghost}/mfa"),
    ] {
        for body in [r#"{"activate":true}"#, r#"{"activate":true,"code":""}"#] {
            both_refuse(
                &http,
                reqwest::Method::PUT,
                &path,
                Some(&admin),
                body,
                400,
                "api.context.invalid_body_param.app_error",
            )
            .await;
        }
    }

    // Past the body checks, an unknown id is `GetUser`'s 404 — on both routes.
    both_refuse(
        &http,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{ghost}/mfa"),
        Some(&admin),
        r#"{"activate":true,"code":"123456"}"#,
        404,
        "app.user.missing_account.const",
    )
    .await;
    both_refuse(
        &http,
        reqwest::Method::POST,
        &format!("/api/v4/users/{ghost}/mfa/generate"),
        Some(&admin),
        "",
        404,
        "app.user.missing_account.const",
    )
    .await;

    // A password account reaches the flag and gets the 501.
    both_refuse(
        &http,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{subject}/mfa"),
        Some(&admin),
        r#"{"activate":true,"code":"123456"}"#,
        501,
        "mfa.mfa_disabled.app_error",
    )
    .await;
    both_refuse(
        &http,
        reqwest::Method::POST,
        &format!("/api/v4/users/{subject}/mfa/generate"),
        Some(&admin),
        "",
        501,
        "mfa.mfa_disabled.app_error",
    )
    .await;

    // Move the account to `gitlab` and the *same* request becomes a 400 instead — the
    // auth-service refusal sits ahead of the flag. `mfa/generate` has no such check and still
    // answers 501, which is what makes this a test of the order rather than of the account.
    let (status, body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{subject}/auth"),
        &admin,
        r#"{"auth_service":"gitlab","auth_data":"mmrsauth-mfagates"}"#,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

    both_refuse(
        &http,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{subject}/mfa"),
        Some(&admin),
        r#"{"activate":true,"code":"123456"}"#,
        400,
        "api.user.activate_mfa.email_and_ldap_only.app_error",
    )
    .await;
    both_refuse(
        &http,
        reqwest::Method::POST,
        &format!("/api/v4/users/{subject}/mfa/generate"),
        Some(&admin),
        "",
        501,
        "mfa.mfa_disabled.app_error",
    )
    .await;

    // And `ldap` is the one non-empty service the activation check lets through, so it is back to
    // the 501. Without this the predicate could be `AuthService != ""` and every assertion above
    // would still pass.
    let (status, _, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{subject}/auth"),
        &admin,
        r#"{"auth_service":"ldap","auth_data":"mmrsauth-mfagates"}"#,
    )
    .await;
    assert_eq!(status, 200);
    both_refuse(
        &http,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{subject}/mfa"),
        Some(&admin),
        r#"{"activate":true,"code":"123456"}"#,
        501,
        "mfa.mfa_disabled.app_error",
    )
    .await;

    scrub("mfagates").await;
}

/// **A plain account cannot reach anybody else's MFA, and reaches its own.**
///
/// `SessionHasPermissionToUser` is the gate, and it is the *second* check — after the id shape,
/// before anything is read. Asserting the self case as well is what stops the gate being read as
/// "administrators only": the owner of an account may generate their own secret, and gets the
/// 501 rather than the 403.
#[tokio::test]
async fn the_mfa_pair_is_self_or_edit_other_users() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let stranger = make_user(&http, &admin, &team, "mfastranger").await;
    make_user(&http, &admin, &team, "mfaowner").await;
    let owner = login(&http, "mfaowner").await;

    both_refuse(
        &http,
        reqwest::Method::POST,
        &format!("/api/v4/users/{stranger}/mfa/generate"),
        Some(&owner),
        "",
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both_refuse(
        &http,
        reqwest::Method::PUT,
        &format!("/api/v4/users/{stranger}/mfa"),
        Some(&owner),
        r#"{"activate":false}"#,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    // Its own account, through the `me` alias, which `RequireUserId` substitutes before the
    // permission check — so this exercises the substitution as well as the gate.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/me/mfa/generate",
        Some(&owner),
        "",
        501,
        "mfa.mfa_disabled.app_error",
    )
    .await;

    scrub("mfastranger").await;
    scrub("mfaowner").await;
}

/// **The deactivation is forwarded, and the forward is taken before anything is written.**
///
/// `DeactivateMfa` has no configuration gate: it writes `MfaActive = false` and `MfaSecret = ''`,
/// bumping `UpdateAt` twice, and then sends an MFA-change e-mail from a goroutine. The e-mail is
/// the part this process cannot reproduce ([D-238]), so the whole request goes to Go — after the
/// `GetUser` whose 404 is served here, and before the first `UPDATE`.
///
/// What proves the forward is `x-mmrs-served-by`: a served answer carries `rust` and a forwarded
/// one carries nothing at all. What proves it precedes the write is the unknown-id case in
/// `the_mfa_refusals_agree_and_keep_their_order`, which is answered here without Go ever seeing
/// it.
#[tokio::test]
async fn deactivating_mfa_is_forwarded_and_still_answers_ok() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "mfaoff").await;

    let before = number_of("mfaoffrs", "updateat").await;

    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/mfa"),
        &admin,
        r#"{"activate":false}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/mfa"),
        &admin,
        r#"{"activate":false}"#,
    )
    .await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body, go_body, "identical bytes");
    assert_eq!(rs_body, br#"{"status":"OK"}"#, "ReturnStatusOK, no newline");
    assert_eq!(
        served.as_deref(),
        Some("go"),
        "forwarded — the proxy stamps `go`, a served answer stamps `rust`"
    );

    // Go did the write, which is the point: the two no-op `UPDATE`s still move `UpdateAt`.
    assert!(
        number_of("mfaoffrs", "updateat").await > before,
        "the forwarded request wrote the row"
    );
    assert_eq!(
        column_of("mfaoffrs", "mfaactive").await.as_deref(),
        Some("false")
    );

    scrub_pair("mfaoff").await;
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/login/switch
// ---------------------------------------------------------------------------------------------

/// **Every pair of services that routes to nothing is the same 400 a malformed body gets.**
///
/// `saml → ldap` is the interesting one: `saml` is in the "OAuth" set for the first two
/// predicates and `ldap` has its own pair, so the request satisfies none of the four. A client
/// cannot tell this from a body that would not parse.
#[tokio::test]
async fn an_unroutable_switch_is_the_invalid_param_four_hundred() {
    if !stack_enabled() {
        return;
    }
    let http = client();

    for body in [
        "{}",
        "notjson",
        "[]",
        r#"{"current_service":"saml","new_service":"ldap"}"#,
        r#"{"current_service":"ldap","new_service":"saml"}"#,
        r#"{"current_service":"email","new_service":"email"}"#,
        r#"{"current_service":"email","new_service":"bogus"}"#,
        r#"{"current_service":"ldap","new_service":"ldap"}"#,
    ] {
        both_refuse(
            &http,
            reqwest::Method::POST,
            "/api/v4/users/login/switch",
            None,
            body,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
    }
}

/// **The two `email → …` branches serve their unknown-address 404 and forward everything past
/// it**, because the next thing each does is `CheckPasswordAndAllCriteria`, which claims a
/// `FailedAttempts` slot before it compares anything.
///
/// The forward is asserted on a *real* account with a wrong password: both servers answer 401,
/// and this one carries no `x-mmrs-served-by` because Go answered. The account's
/// `FailedAttempts` moves by exactly one, which is the evidence that the slot was claimed once
/// rather than twice — a forward taken after the claim would show two.
#[tokio::test]
async fn the_email_switch_branches_serve_the_404_and_forward_the_password_check() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    make_user(&http, &admin, &team, "switchemail").await;

    for new_service in ["gitlab", "ldap"] {
        both_refuse(
            &http,
            reqwest::Method::POST,
            "/api/v4/users/login/switch",
            None,
            &format!(
                r#"{{"current_service":"email","new_service":"{new_service}",
                     "email":"mmrsauth-nosuch@mmrs.invalid","password":"x","ldap_id":"l"}}"#
            ),
            404,
            "app.user.missing_account.const",
        )
        .await;
    }

    let before = number_of("switchemail", "failedattempts").await;
    let (status, body, served) = post(
        &http,
        RUST,
        "/api/v4/users/login/switch",
        None,
        &format!(
            r#"{{"current_service":"email","new_service":"gitlab",
                 "email":"{}","password":"definitely-wrong"}}"#,
            email("switchemail")
        ),
    )
    .await;
    assert_eq!(status, 401, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("JSON")["id"].as_str(),
        Some("api.user.check_user_password.invalid.app_error")
    );
    assert_eq!(
        served.as_deref(),
        Some("go"),
        "forwarded before the claim — the proxy stamps `go`"
    );
    assert_eq!(
        number_of("switchemail", "failedattempts").await,
        before + 1,
        "exactly one failed-attempt slot was claimed, by Go"
    );

    scrub("switchemail").await;
}

/// **`… → email` demands a session first, and then refuses an account that does not use SSO.**
///
/// The session requirement is inside the branch, not on the handler: the same route with no
/// credentials at all answers the 400 above for an unroutable pair and the **401** here. Both are
/// asserted, because a port that put the session on the handler would turn the first into a 401.
#[tokio::test]
async fn oauth_to_email_needs_a_session_and_then_the_account_to_be_sso() {
    if !stack_enabled() {
        return;
    }
    // Both branches read the licence: licensed, they forward instead of refusing. A
    // sibling suite planting an active licence row would turn every assertion below
    // into a forward, so this holds the unlicensed side of that lock for the whole test.
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    make_user(&http, &admin, &team, "switchback").await;
    let owner = login(&http, "switchback").await;

    let body = format!(
        r#"{{"current_service":"gitlab","new_service":"email","email":"{}",
             "new_password":"Mmrs-Switched-1234"}}"#,
        email("switchback")
    );

    // No token: `c.SessionRequired()` inside the branch.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        None,
        &body,
        401,
        "api.context.session_expired.app_error",
    )
    .await;

    // A session, but the address belongs to somebody else.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        Some(&admin),
        &body,
        403,
        "api.user.oauth_to_email.context.app_error",
    )
    .await;

    // The owner's own session, but the account authenticates by password.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        Some(&owner),
        &body,
        400,
        "api.user.oauth_to_email.not_oauth_user.app_error",
    )
    .await;

    // And an address nobody holds is the 404, before the id-mismatch 403 — so the route cannot be
    // used to enumerate addresses.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        Some(&owner),
        r#"{"current_service":"gitlab","new_service":"email",
            "email":"mmrsauth-nosuch@mmrs.invalid","new_password":"Mmrs-Switched-1234"}"#,
        404,
        "app.user.missing_account.const",
    )
    .await;

    assert!(
        can_still_log_in(&http, "switchback").await,
        "no refusal changed the password"
    );

    scrub("switchback").await;
}

/// **`ldap → email` is served whole, and terminates at a 501 nothing in this build can get past.**
///
/// `RegisterLdapInterface` is called only from the enterprise import package, which is not in the
/// pinned tree, so `a.Ldap()` is nil and the branch stops there for every account that clears the
/// earlier refusals. The `not_ldap_account` 400 is the refusal ahead of it, and it reads the
/// **stored** `AuthService` rather than the body's `current_service` — so an account that is not
/// LDAP is refused even though the body said it was, which is what the two cases here separate.
#[tokio::test]
async fn ldap_to_email_refuses_a_non_ldap_account_and_then_has_no_ldap() {
    if !stack_enabled() {
        return;
    }
    // Both branches read the licence: licensed, they forward instead of refusing. A
    // sibling suite planting an active licence row would turn every assertion below
    // into a forward, so this holds the unlicensed side of that lock for the whole test.
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let id = make_user(&http, &admin, &team, "switchldap").await;

    let body = format!(
        r#"{{"current_service":"ldap","new_service":"email","email":"{}",
             "password":"whatever","new_password":"Mmrs-Switched-1234"}}"#,
        email("switchldap")
    );

    // A password account claiming to be LDAP.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        None,
        &body,
        400,
        "api.user.ldap_to_email.not_ldap_account.app_error",
    )
    .await;

    // Make it genuinely LDAP, through the route this session also ships.
    let (status, response, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{id}/auth"),
        &admin,
        r#"{"auth_service":"ldap","auth_data":"mmrsauth-switchldap"}"#,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response));

    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        None,
        &body,
        501,
        "api.user.ldap_to_email.not_available.app_error",
    )
    .await;

    // An unknown address is the 404, ahead of both.
    both_refuse(
        &http,
        reqwest::Method::POST,
        "/api/v4/users/login/switch",
        None,
        r#"{"current_service":"ldap","new_service":"email",
            "email":"mmrsauth-nosuch@mmrs.invalid","password":"x","new_password":"Mmrs-Sw-1234"}"#,
        404,
        "app.user.missing_account.const",
    )
    .await;

    scrub("switchldap").await;
}
