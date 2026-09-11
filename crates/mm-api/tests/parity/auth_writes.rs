//! Cross-server parity for the five authentication write routes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity auth_writes
//! ```
//!
//! # Every destructive test uses a throwaway user, and that rule is not stylistic
//!
//! `PUT /users/{id}/password` with `already_hashed=true` writes its argument into
//! `Users.Password` **verbatim**, and an admin has the permission for it. Running that probe
//! against the shared fixture user during development left the whole suite unable to log in — the
//! account's password became the literal string it was handed. Recovered through Go's own reset
//! flow, which is also what the fixtures below use to mint tokens.
//!
//! # Tokens are minted by Go and read out of the table
//!
//! `POST /users/password/reset/send` and `/users/email/verify/send` are forwarded, not ported
//! ([D-219]) — they exist only to send an e-mail. They still **save the token before trying to
//! send**, so calling Go's route and then reading `Tokens` gives a token minted by the oracle, in
//! the oracle's own `Extra` encoding, rather than one this suite invented. The password-reset
//! route answers 500 on a stack with no SMTP and the verification route answers 200; both leave
//! the row.
//!
//! # Reads go back through the server that wrote
//!
//! [D-190]: a write served by mm-api leaves Go's caches stale. Every "did it take effect"
//! assertion here logs in against the *same* base that performed the write.

use std::time::Duration;

use crate::common;

use common::{
    GO, PLAIN_USER_PASSWORD, RUST, SocketProbe, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, delete_plain_user,
    go_minted_token, logged_in_user_id, plain_username, post_both_raw, stack_enabled,
};

/// A 64-character token that is syntactically valid and names nothing.
fn bogus_token() -> String {
    "q".repeat(64)
}

/// The shared fixture pool — capped acquire timeout, one connection, [`None`] without a
/// `DATABASE_URL`.
async fn pool() -> Option<sqlx::PgPool> {
    common::fixture_pool().await
}

/// `Users.EmailVerified`, read from the table.
///
/// **Not readable over the API.** `SanitizeProfile` clears `EmailVerified` for anyone without
/// `manage_system` over the target, and the field is `omitempty`, so `GET /users/{id}` answers
/// with the key absent whether the flag is set or not. A test that asserted through the route
/// would pass against a handler that did nothing.
async fn email_verified(user_id: &str) -> bool {
    let Some(pool) = pool().await else {
        return false;
    };
    sqlx::query_scalar::<_, Option<bool>>("SELECT emailverified FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("the user exists")
        .unwrap_or(false)
}

/// Ask Go to mint a one-shot token for `email` and hand back the row it saved.
///
/// `kind` is the route segment — `password/reset` or `email/verify`. The status is ignored on
/// purpose: the password route fails at the send step with a 500 on a stack with no mail server,
/// and the token is already in the table by then.
async fn mint_token(http: &reqwest::Client, kind: &str, email: &str, token_type: &str) -> String {
    let _ = http
        .post(format!("{GO}/api/v4/users/{kind}/send"))
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .expect("Go answers");

    let pool = pool().await.expect("DATABASE_URL for the token fixture");
    let row: (String,) = sqlx::query_as(
        "SELECT token FROM tokens WHERE type = $1 AND extra LIKE $2 ORDER BY createat DESC LIMIT 1",
    )
    .bind(token_type)
    .bind(format!("%{email}%"))
    .fetch_one(&pool)
    .await
    .unwrap_or_else(|e| panic!("Go minted no {token_type} token for {email}: {e}"));
    row.0
}

/// Whether a token row still exists — the assertion that consuming it deleted it.
async fn token_exists(token: &str) -> bool {
    let Some(pool) = pool().await else {
        return false;
    };
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tokens WHERE token = $1")
        .bind(token)
        .fetch_one(&pool)
        .await
        .unwrap_or(0)
        > 0
}

/// `Users.FailedAttempts`, read straight from the table. The counter has no route that reports it.
async fn failed_attempts(user_id: &str) -> i32 {
    let Some(pool) = pool().await else {
        return -1;
    };
    sqlx::query_scalar::<_, i32>("SELECT failedattempts FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("the user exists")
}

/// Set a user's `AuthService` directly. There is no route that does this to an existing account,
/// and `reset_failed_attempts` branches on it.
async fn set_auth_service(user_id: &str, auth_service: &str) {
    let Some(pool) = pool().await else { return };
    sqlx::query("UPDATE users SET authservice = $1 WHERE id = $2")
        .bind(auth_service)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("sets the auth service");
}

/// Log in against one base and return `(status, token)`.
async fn login(
    http: &reqwest::Client,
    base: &str,
    login_id: &str,
    password: &str,
) -> (u16, String) {
    let response = http
        .post(format!("{base}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": login_id, "password": password }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let token = response
        .headers()
        .get("token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (status, token)
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

/// The two anonymous cases, which are the whole reason this route is an `APIHandler`: no token and
/// a token that names nothing are both a **200**, with the same body and the same `Set-Cookie`.
#[tokio::test]
async fn logout_answers_two_hundred_to_a_caller_with_no_usable_session() {
    if !stack_enabled() {
        return;
    }
    let http = client();

    for auth in [None, Some("notarealtokenatall")] {
        let mut bodies = Vec::new();
        let mut cookies = Vec::new();
        for base in [GO, RUST] {
            let mut request = http.post(format!("{base}/api/v4/users/logout"));
            if let Some(token) = auth {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = request.send().await.expect("both servers answer");
            assert_eq!(response.status(), 200, "{base} with auth={auth:?}");
            if base == RUST {
                common::assert_served_by_rust(response.headers(), "/api/v4/users/logout");
            }
            cookies.push(
                response
                    .headers()
                    .get("set-cookie")
                    .expect("the cookie is cleared unconditionally")
                    .to_str()
                    .expect("ASCII")
                    .to_owned(),
            );
            bodies.push(response.bytes().await.expect("a body").to_vec());
        }
        assert_eq!(bodies[0], bodies[1], "auth={auth:?}");
        assert_eq!(
            bodies[1], br#"{"status":"OK"}"#,
            "ReturnStatusOK, and no trailing newline"
        );
        assert_eq!(
            cookies[0], cookies[1],
            "`MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly`, attribute for attribute — auth={auth:?}"
        );
    }
}

/// A session revoked by mm-api is gone here immediately — and **is still accepted by Go** until
/// Go's session cache is invalidated.
///
/// That second half is the finding, and it is measured rather than assumed: Go's
/// `PlatformService` memoises sessions by token, our `DELETE` does not reach that map, and a user
/// who logs out through mm-api stays authenticated against the Go server for the life of the
/// cache entry. It is [D-190]'s class with a credential consequence, recorded separately as
/// [D-236]. The test pins both halves so that neither can change without somebody noticing.
#[tokio::test]
async fn a_session_revoked_here_is_gone_here_but_lingers_in_gos_cache() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let (team, _channel) =
        a_team_and_channel_the_user_is_in(&http, &go_minted_token(&http).await).await;
    let admin = go_minted_token(&http).await;
    let user = create_plain_user(&http, &admin, &team, "logoutrevoke").await;

    let (status, token) = login(
        &http,
        GO,
        &plain_username("logoutrevoke"),
        PLAIN_USER_PASSWORD,
    )
    .await;
    assert_eq!(status, 200);

    let response = http
        .post(format!("{RUST}/api/v4/users/logout"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("mm-api answers");
    assert_eq!(response.status(), 200);
    common::assert_served_by_rust(response.headers(), "/api/v4/users/logout");

    let me = async |base: &str| {
        http.get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("answers")
            .status()
            .as_u16()
    };

    assert_eq!(me(RUST).await, 401, "the row is gone and we do not cache it");
    assert_eq!(
        me(GO).await,
        200,
        "D-236: Go's session cache still holds the revoked session — if this ever becomes 401, \
         the cache is being invalidated and the entry can be closed"
    );

    common::invalidate_go_caches(&http, &admin).await;
    assert_eq!(me(GO).await, 401, "and the row really is gone");

    delete_plain_user(&http, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------
// PUT /users/{user_id}/password
// ---------------------------------------------------------------------------

/// Six refusals, byte-compared. The interesting ones are the last two: a well-formed id that names
/// nobody is a **403**, not a 404 — `canUpdatePassword` is left false by the failed fetch — and an
/// unparseable body is the *`current_password`* 400 rather than a decode error, because
/// `MapFromJSON` swallows everything.
#[tokio::test]
async fn every_update_password_refusal_matches_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id();

    let cases: Vec<(&str, String, Vec<u8>)> = vec![
        (
            "wrong current password is a 400 with its own id, not the 401 the check produced",
            format!("/api/v4/users/{me}/password"),
            br#"{"current_password":"definitely-not-it","new_password":"Slice-Test-1234"}"#
                .to_vec(),
        ),
        (
            "an absent current_password on the self path is an invalid body param",
            format!("/api/v4/users/{me}/password"),
            br#"{"new_password":"Slice-Test-1234"}"#.to_vec(),
        ),
        (
            "a short new password fails IsPasswordValid after the current one is verified",
            format!("/api/v4/users/{me}/password"),
            br#"{"current_password":"Slice-Test-1234","new_password":"ab"}"#.to_vec(),
        ),
        (
            "a malformed id is a 400 from RequireUserId",
            "/api/v4/users/notanid/password".to_owned(),
            br#"{"new_password":"whatever12"}"#.to_vec(),
        ),
        (
            "a well-formed id naming nobody is a 403, not a 404",
            "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa/password".to_owned(),
            br#"{"current_password":"x","new_password":"whatever12"}"#.to_vec(),
        ),
        (
            "an unparseable body is the current_password 400",
            format!("/api/v4/users/{me}/password"),
            b"this is not json".to_vec(),
        ),
    ];

    for (what, path, body) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            put_both(&http, &token, &path, &body).await;
        assert_eq!(go_status, rs_status, "{what}: status");
        assert_ne!(go_status, 200, "{what}: this case must be a refusal");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);
    }
}

/// The `already_hashed` branch a caller without the permission takes, and the thing about it a
/// reader would most plausibly "fix": **self is a 401, somebody else is a 403.**
#[tokio::test]
async fn already_hashed_without_permission_is_401_for_self_and_403_for_another() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "hashedperm").await;

    let ((go_status, go_body), (rs_status, rs_body)) = put_both(
        &http,
        &user.token,
        &format!("/api/v4/users/{}/password", user.id),
        br#"{"already_hashed":"true","new_password":"not-a-real-hash"}"#,
    )
    .await;
    assert_eq!(go_status, 401, "self + already_hashed is a 401");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "already_hashed self");

    let ((go_status, go_body), (rs_status, rs_body)) = put_both(
        &http,
        &user.token,
        &format!("/api/v4/users/{}/password", logged_in_user_id()),
        br#"{"already_hashed":"true","new_password":"not-a-real-hash"}"#,
    )
    .await;
    assert_eq!(go_status, 403, "somebody else + already_hashed is a 403");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "already_hashed other");

    delete_plain_user(&http, &admin, &user.id).await;
}

/// The write actually takes: a plain user changes their own password through **mm-api**, the old
/// one stops working and the new one starts, checked against both servers.
#[tokio::test]
async fn a_self_service_password_change_takes_effect_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "pwchange").await;
    let username = plain_username("pwchange");
    let new_password = "Mmrs-Changed-5678";

    let response = http
        .put(format!("{RUST}/api/v4/users/{}/password", user.id))
        .header("Authorization", format!("Bearer {}", user.token))
        .body(format!(
            r#"{{"current_password":"{PLAIN_USER_PASSWORD}","new_password":"{new_password}"}}"#
        ))
        .send()
        .await
        .expect("mm-api answers");
    assert_eq!(response.status(), 200);
    common::assert_served_by_rust(response.headers(), "/api/v4/users/{user_id}/password");
    assert_eq!(
        response.bytes().await.expect("a body").to_vec(),
        br#"{"status":"OK"}"#
    );

    // `POST /users/login` is forwarded, so the only server that can verify a password is Go —
    // and Go answers it from a user cache our write did not touch ([D-190]). Without this the
    // old password still works and the new one does not, which looks like a broken write and is
    // a stale read. Measured.
    common::invalidate_go_caches(&http, &admin).await;

    assert_eq!(
        login(&http, GO, &username, PLAIN_USER_PASSWORD).await.0,
        401,
        "the old password still works after a change mm-api made"
    );
    assert_eq!(
        login(&http, GO, &username, new_password).await.0,
        200,
        "the new password does not work"
    );

    // A successful check zeroes the counter, which the failed login above had bumped.
    assert_eq!(failed_attempts(&user.id).await, 0);

    delete_plain_user(&http, &admin, &user.id).await;
}

/// The failed-attempt counter is the lockout, and its arithmetic is observable only here.
///
/// A wrong current password **consumes** a slot. A too-short *new* password does not — the current
/// one verified, so the counter is zeroed before `IsPasswordValid` even runs. Both servers are
/// asked to do the same thing to the same row, one after the other.
#[tokio::test]
async fn a_wrong_current_password_consumes_an_attempt_and_a_right_one_clears_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "pwattempts").await;
    let path = format!("/api/v4/users/{}/password", user.id);

    for (base, expected) in [(GO, 1), (RUST, 2)] {
        let response = http
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {}", user.token))
            .body(r#"{"current_password":"wrong-one","new_password":"Mmrs-Whatever-1"}"#)
            .send()
            .await
            .expect("answers");
        assert_eq!(response.status(), 400, "{base}");
        assert_eq!(
            failed_attempts(&user.id).await,
            expected,
            "{base} must consume exactly one attempt for a credential mismatch"
        );
    }

    // The right current password with a *refused* new one still clears the counter: the claim is
    // released before `IsPasswordValid` is consulted.
    let response = http
        .put(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {}", user.token))
        .body(format!(
            r#"{{"current_password":"{PLAIN_USER_PASSWORD}","new_password":"ab"}}"#
        ))
        .send()
        .await
        .expect("answers");
    assert_eq!(response.status(), 400, "the new password is too short");
    assert_eq!(
        failed_attempts(&user.id).await,
        0,
        "a verified current password clears the counter even when the write is then refused"
    );

    delete_plain_user(&http, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------
// POST /users/password/reset
// ---------------------------------------------------------------------------

/// The three refusals, byte-compared. A 63-character token is a *body param* 400; a 64-character
/// one that names nothing is `invalid_link`. The two are different ids on purpose — neither says
/// whether any token exists.
#[tokio::test]
async fn every_reset_password_refusal_matches_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "a short token is an invalid body param",
            br#"{"token":"abc","new_password":"Mmrs-Whatever-1"}"#.to_vec(),
        ),
        (
            "an empty body is the same, because MapFromJSON yields no token",
            Vec::new(),
        ),
        (
            "a well-formed token naming nothing is invalid_link",
            format!(
                r#"{{"token":"{}","new_password":"Mmrs-Whatever-1"}}"#,
                bogus_token()
            )
            .into_bytes(),
        ),
    ];

    for (what, body) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, "/api/v4/users/password/reset", &body).await;
        assert_eq!(go_status, rs_status, "{what}");
        assert_ne!(go_status, 200, "{what}: this case must be a refusal");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);
    }
}

/// A real token, consumed by **mm-api**: the password changes, the row is deleted, and the same
/// token a second time is `invalid_link` on both servers.
#[tokio::test]
async fn a_reset_token_changes_the_password_and_is_consumed() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "pwreset").await;
    let username = plain_username("pwreset");
    let email = format!("{username}@mmrs.invalid");
    let new_password = "Mmrs-Reset-9012";

    let token = mint_token(&http, "password/reset", &email, "password_recovery").await;
    assert_eq!(token.len(), 64);

    let response = http
        .post(format!("{RUST}/api/v4/users/password/reset"))
        .body(format!(
            r#"{{"token":"{token}","new_password":"{new_password}"}}"#
        ))
        .send()
        .await
        .expect("mm-api answers");
    assert_eq!(response.status(), 200);
    common::assert_served_by_rust(response.headers(), "/api/v4/users/password/reset");

    // Go answers `login` from its user cache; see the note in the self-service test.
    common::invalidate_go_caches(&http, &admin).await;
    assert_eq!(
        login(&http, GO, &username, new_password).await.0,
        200,
        "Go does not accept the password mm-api set"
    );
    assert!(
        !token_exists(&token).await,
        "a consumed reset token must be deleted"
    );

    // The second use is the same refusal on both servers.
    let body = format!(r#"{{"token":"{token}","new_password":"Mmrs-Again-3456"}}"#).into_bytes();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &admin, "/api/v4/users/password/reset", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a spent reset token");

    delete_plain_user(&http, &admin, &user.id).await;
}

/// A reset token whose `Extra` no longer matches the account's e-mail is `link_expired` — the
/// **same id an actually expired token gets**, which is why the mismatch is worth pinning
/// separately. Arranged by moving the token's `CreateAt` past the 24-hour recovery window, which
/// no route can do.
#[tokio::test]
async fn an_expired_reset_token_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "pwexpired").await;
    let email = format!("{}@mmrs.invalid", plain_username("pwexpired"));

    let token = mint_token(&http, "password/reset", &email, "password_recovery").await;
    let Some(pool) = pool().await else {
        delete_plain_user(&http, &admin, &user.id).await;
        return;
    };
    // 25 hours ago: past `PasswordRecoverExpiryTime` (24h) and inside `MaxTokenExipryTime` (48h),
    // so a port that used the wrong window would still accept it.
    sqlx::query("UPDATE tokens SET createat = createat - $1 WHERE token = $2")
        .bind(25_i64 * 60 * 60 * 1000)
        .bind(&token)
        .execute(&pool)
        .await
        .expect("ages the token");

    let body = format!(r#"{{"token":"{token}","new_password":"Mmrs-Expired-1"}}"#).into_bytes();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &admin, "/api/v4/users/password/reset", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an expired reset token");
    assert!(
        token_exists(&token).await,
        "a refused token is NOT deleted — the link stays retryable"
    );

    let _ = sqlx::query("DELETE FROM tokens WHERE token = $1")
        .bind(&token)
        .execute(&pool)
        .await;
    delete_plain_user(&http, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------
// POST /users/email/verify
// ---------------------------------------------------------------------------

/// Two refusals, and the second is the one that matters: **every** app-layer error becomes the
/// same 400 `bad_link`, so a token of the wrong type is indistinguishable from one that never
/// existed.
#[tokio::test]
async fn every_verify_email_refusal_matches_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "a short token is an invalid body param",
            br#"{"token":"abc"}"#.to_vec(),
        ),
        (
            "a well-formed token naming nothing is bad_link",
            format!(r#"{{"token":"{}"}}"#, bogus_token()).into_bytes(),
        ),
    ];

    for (what, body) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&http, &token, "/api/v4/users/email/verify", &body).await;
        assert_eq!(go_status, rs_status, "{what}");
        assert_ne!(go_status, 200, "{what}: this case must be a refusal");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);
    }
}

/// A **password-recovery** token offered to the verification route is `bad_link`, not
/// `invalid_link` — the type check is the second half of `GetVerifyEmailToken` and this is the
/// only way to reach it.
#[tokio::test]
async fn a_token_of_the_wrong_type_is_bad_link_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "wrongtype").await;
    let email = format!("{}@mmrs.invalid", plain_username("wrongtype"));

    let token = mint_token(&http, "password/reset", &email, "password_recovery").await;
    let body = format!(r#"{{"token":"{token}"}}"#).into_bytes();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &admin, "/api/v4/users/email/verify", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a recovery token at verify");
    assert!(
        token_exists(&token).await,
        "a refused token is not consumed"
    );

    if let Some(pool) = pool().await {
        let _ = sqlx::query("DELETE FROM tokens WHERE token = $1")
            .bind(&token)
            .execute(&pool)
            .await;
    }
    delete_plain_user(&http, &admin, &user.id).await;
}

/// The full success path, plus the websocket events. `VerifyUserEmail` calls
/// `sendUpdatedUserEvent`, which publishes **three** `user_updated` frames carrying three
/// differently sanitised copies of the same user — dropping them would leave every connected
/// client believing the address is still unverified, with nothing to correct it.
#[tokio::test]
async fn verifying_an_email_publishes_user_updated_and_consumes_the_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "verifyok").await;
    let email = format!("{}@mmrs.invalid", plain_username("verifyok"));

    let token = mint_token(&http, "email/verify", &email, "verify_email").await;

    // The admin watches: `sendUpdatedUserEvent` omits the subject from the broadcast, so the
    // subject's own socket would see only the third frame.
    let mut probe = SocketProbe::connect(RUST, &admin).await;

    let response = http
        .post(format!("{RUST}/api/v4/users/email/verify"))
        .body(format!(r#"{{"token":"{token}"}}"#))
        .send()
        .await
        .expect("mm-api answers");
    assert_eq!(response.status(), 200);
    common::assert_served_by_rust(response.headers(), "/api/v4/users/email/verify");
    assert_eq!(
        response.bytes().await.expect("a body").to_vec(),
        br#"{"status":"OK"}"#
    );

    probe.collect_for(Duration::from_millis(900)).await;
    let frames: Vec<_> = probe
        .events_named("user_updated")
        .into_iter()
        .filter(|frame| frame["data"]["user"]["id"].as_str() == Some(user.id.as_str()))
        .collect();
    assert!(
        !frames.is_empty(),
        "no user_updated reached a watching admin: {:?}",
        probe.frames()
    );

    assert!(
        email_verified(&user.id).await,
        "the flag did not move — see `email_verified` for why this is not read over the API"
    );

    assert!(
        !token_exists(&token).await,
        "a consumed verification token must be deleted"
    );

    delete_plain_user(&http, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------
// POST /users/{user_id}/reset_failed_attempts
// ---------------------------------------------------------------------------

/// The success, and the two refusals with two different ids — the permission one is hand-built
/// with its own id, and the auth-service one is a 400.
#[tokio::test]
async fn reset_failed_attempts_matches_go_on_every_branch() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "unlock").await;
    let path = format!("/api/v4/users/{}/reset_failed_attempts", user.id);

    // A plain caller lacks `sysconsole_write_user_management_users`: the hand-built 403.
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &user.token, &path, b"").await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a plain caller is refused");

    // A user id naming nobody, with the permission: `get_user`'s own 404, not the 403 above.
    let ((go_status, go_body), (rs_status, rs_body)) = post_both_raw(
        &http,
        &admin,
        "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa/reset_failed_attempts",
        b"",
    )
    .await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an unknown user");

    // The write: bump the counter behind the API's back, then clear it through mm-api.
    if let Some(pool) = pool().await {
        sqlx::query("UPDATE users SET failedattempts = 5 WHERE id = $1")
            .bind(&user.id)
            .execute(&pool)
            .await
            .expect("locks the account");
    }
    let response = http
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("mm-api answers");
    assert_eq!(response.status(), 200);
    common::assert_served_by_rust(
        response.headers(),
        "/api/v4/users/{user_id}/reset_failed_attempts",
    );
    assert_eq!(failed_attempts(&user.id).await, 0);

    // An SSO account has no local counter to clear: a 400 with its own id. `""` and `ldap` pass;
    // everything else, `email` included, does not.
    set_auth_service(&user.id, "gitlab").await;
    // Go reads the user through its cache, which still holds the pre-update row ([D-190]).
    common::invalidate_go_caches(&http, &admin).await;
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&http, &admin, &path, b"").await;
    assert_eq!(go_status, 400, "an SSO account cannot be unlocked");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an SSO account");
    set_auth_service(&user.id, "").await;
    common::invalidate_go_caches(&http, &admin).await;

    delete_plain_user(&http, &admin, &user.id).await;
}

/// `PUT` is not a method [`post_both_raw`] covers; the same bracket, one verb along.
async fn put_both(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let put = async |base: &str| {
        let response = http
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            common::assert_served_by_rust(response.headers(), path);
        }
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (put(GO).await, put(RUST).await)
}
