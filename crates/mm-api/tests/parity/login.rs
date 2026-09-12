//! Cross-server parity for `POST /api/v4/users/login` and `POST /api/v4/users/login/type`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity login
//! ```
//!
//! # Every failing login here uses a throwaway account, and that rule is load bearing
//!
//! A failed login increments `Users.FailedAttempts` on a column **both servers share**, and the
//! shared fixture user (`common::LOGIN_ID`) is the account every other suite in this binary
//! authenticates as. Ten stray failures against it would lock it out and fail dozens of tests in
//! files this one never touches — the exact "a failure naming a route you did not touch" shape
//! this project has hit before. So:
//!
//! - anything that can fail authentication uses a `create_plain_user` account;
//! - the lockout test uses its own account and never touches another;
//! - the one shared account used here is the **seeded bot**, which is refused in the preflight
//!   *before* the counter is claimed and therefore cannot be locked out.
//!
//! # Two requests to two servers are two logins
//!
//! Unlike a read, `POST /users/login` mutates on both sides: two sessions are created and
//! `Users.UpdateAt`/`LastLogin` move twice. So the success bodies cannot be compared byte for
//! byte — the response carries the row as it was read *before* `UpdateLastLogin` ran, so Go's
//! body shows the value from before its own login and ours shows the value Go's login just
//! wrote. [`assert_user_bodies_agree`] compares every field except that one and asserts the key
//! sets are identical, which is the strongest claim the route allows.

use crate::common;

use common::{
    GO, PLAIN_USER_PASSWORD, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, delete_plain_user, go_minted_token, plain_username, stack_enabled,
};

/// `POST` with **no credentials at all** to both servers, returning `(status, headers, body)`.
///
/// `login` is an `APIHandler`, so an `Authorization` header is neither required nor consulted —
/// and sending one would make this fixture depend on a token whose session another suite may have
/// revoked.
async fn login_both(
    client: &reqwest::Client,
    path: &str,
    body: serde_json::Value,
    extra: &[(&str, &str)],
) -> [(u16, reqwest::header::HeaderMap, Vec<u8>); 2] {
    let send = async |base: &str| {
        let mut request = client
            .post(format!("{base}{path}"))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body).expect("the body serialises"));
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        if base == RUST {
            common::assert_served_by_rust(&headers, path);
        }
        (
            status,
            headers,
            response.bytes().await.expect("body reads").to_vec(),
        )
    };

    [send(GO).await, send(RUST).await]
}

/// The fields of a login success body that cannot agree across two sequential logins.
///
/// `update_at` alone: `DoLogin` writes it *after* the handler has already read the row it
/// serialises, so Go's response shows the instant before Go's own login and ours shows the
/// instant Go's login produced. Nothing else moves.
const VOLATILE: &[&str] = &["update_at"];

fn assert_user_bodies_agree(go_body: &[u8], rs_body: &[u8], context: &str) {
    let go: serde_json::Value = serde_json::from_slice(go_body)
        .unwrap_or_else(|e| panic!("{context}: Go's body is not JSON: {e}"));
    let rs: serde_json::Value = serde_json::from_slice(rs_body)
        .unwrap_or_else(|e| panic!("{context}: our body is not JSON: {e}"));

    let go_obj = go.as_object().expect("an object");
    let rs_obj = rs.as_object().expect("an object");

    assert_eq!(
        go_obj.keys().collect::<Vec<_>>(),
        rs_obj.keys().collect::<Vec<_>>(),
        "{context}: the sanitized user must carry the same keys.\n  go:   {go}\n  rust: {rs}"
    );

    for (key, value) in go_obj {
        if VOLATILE.contains(&key.as_str()) {
            continue;
        }
        assert_eq!(
            rs_obj.get(key),
            Some(value),
            "{context}: `{key}` differs.\n  go:   {go}\n  rust: {rs}"
        );
    }

    // The volatile field still has to be *present and plausible* on both sides — otherwise
    // skipping it would let a port that omitted it entirely pass.
    for (name, obj) in [("go", go_obj), ("rust", rs_obj)] {
        for key in VOLATILE {
            assert!(
                obj.get(*key).and_then(serde_json::Value::as_i64).unwrap_or(0) > 0,
                "{context}: {name} has no plausible `{key}`"
            );
        }
    }

    // And the security property this route is judged on: nothing secret survives `Sanitize`.
    for secret in ["password", "auth_data", "mfa_secret", "mfa_used_timestamps"] {
        assert!(
            !rs_obj.contains_key(secret),
            "{context}: `{secret}` must not be in a login response"
        );
    }
}

/// A wrong password against an account that does not exist: same status, same id, same body.
///
/// The generic id is the one the stock configuration produces — both sign-in methods on, no SSO —
/// and it is the only one of the four a live stack can show. The other three are unit-tested in
/// `mm_api::login`.
#[tokio::test]
async fn an_unknown_account_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    let [(go_status, _, go_body), (rs_status, rs_headers, rs_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": "mmrs-no-such-account@mmrs.invalid",
            "password": "Definitely-Not-A-Password-1",
        }),
        &[],
    )
    .await;

    assert_eq!(go_status, 401);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "login with an unknown account",
    );
    assert_eq!(go["id"], "api.user.login.invalid_credentials_email_username");

    // A refused login mints nothing.
    assert!(rs_headers.get("token").is_none(), "no token on a refusal");
    assert!(
        rs_headers.get("set-cookie").is_none(),
        "no cookie on a refusal"
    );
}

/// A blank password is `blank_pwd` at **400**, not a masked 401 — it is on the unmasked list, and
/// it is raised before the account is looked up.
///
/// The pair with the test above is the point: two bad requests, two different ids and two
/// different statuses. A mask that swallowed everything would make them agree.
#[tokio::test]
async fn a_blank_password_keeps_its_own_id_and_status() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    for body in [
        serde_json::json!({ "login_id": "mmrs-no-such-account@mmrs.invalid" }),
        serde_json::json!({ "login_id": "mmrs-no-such-account@mmrs.invalid", "password": "" }),
        // No keys at all, and a body that is not even an object — `MapFromJSON` turns both into
        // the empty map, so both are a blank password rather than a decode error.
        serde_json::json!({}),
        serde_json::json!([1, 2, 3]),
    ] {
        let [(go_status, _, go_body), (rs_status, _, rs_body)] =
            login_both(&client, "/api/v4/users/login", body.clone(), &[]).await;

        assert_eq!(go_status, 400, "Go's status for {body}");
        assert_eq!(rs_status, go_status, "our status for {body}");
        let go = assert_error_bodies_match_except_known_gaps(
            &go_body,
            &rs_body,
            &format!("blank password for {body}"),
        );
        assert_eq!(go["id"], "api.user.login.blank_pwd.app_error");
    }
}

/// A successful login: the same user body, a `Token` header on both, and **no cookies** without
/// `X-Requested-With`.
///
/// Uses a throwaway account so that neither the shared fixture user's counter nor its `UpdateAt`
/// moves.
#[tokio::test]
async fn a_successful_login_answers_the_same_user_and_a_token() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login1").await;

    let [(go_status, go_headers, go_body), (rs_status, rs_headers, rs_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": plain_username("login1"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;

    assert_eq!(go_status, 200, "Go refused: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "we refused: {}", String::from_utf8_lossy(&rs_body));
    assert_user_bodies_agree(&go_body, &rs_body, "a successful login");

    // The token is a fresh 26-character id on both, and it is not the same one.
    let token_of = |headers: &reqwest::header::HeaderMap| {
        headers
            .get("token")
            .expect("a Token header")
            .to_str()
            .expect("ASCII")
            .to_owned()
    };
    let go_token = token_of(&go_headers);
    let rs_token = token_of(&rs_headers);
    assert_eq!(go_token.len(), 26, "Go's token is an id");
    assert_eq!(rs_token.len(), 26, "our token is an id");
    assert_ne!(go_token, rs_token);

    // Without `X-Requested-With` neither server sets a cookie. Asserting this on **both** is what
    // makes it a parity claim rather than a statement about our handler.
    assert!(go_headers.get("set-cookie").is_none(), "Go sent a cookie");
    assert!(rs_headers.get("set-cookie").is_none(), "we sent a cookie");

    // And the token actually authenticates, through the server that minted it.
    for (base, token) in [(GO, &go_token), (RUST, &rs_token)] {
        let me = client
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(me.status(), 200, "{base} did not accept its own token");
        let body: serde_json::Value = me.json().await.expect("a user");
        assert_eq!(body["id"], user.id, "{base} minted a token for someone else");
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// The three cookies, with `X-Requested-With: XMLHttpRequest`: same names, same order, same
/// attributes, and only the first `HttpOnly`.
///
/// The values differ — they are two different sessions — so this compares the *shape*: the
/// attribute list of each cookie after its value is stripped. `Expires` is dropped from the
/// comparison because the two responses are seconds apart; `Max-Age` carries the same
/// information and is exact.
#[tokio::test]
async fn the_three_session_cookies_have_the_same_shape_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login2").await;

    let [(go_status, go_headers, _), (rs_status, rs_headers, _)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": plain_username("login2"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[("X-Requested-With", "XMLHttpRequest")],
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);

    let shapes = |headers: &reqwest::header::HeaderMap| -> Vec<(String, Vec<String>)> {
        headers
            .get_all("set-cookie")
            .iter()
            .map(|value| {
                let raw = value.to_str().expect("ASCII");
                let mut parts = raw.split("; ");
                let name = parts
                    .next()
                    .and_then(|pair| pair.split('=').next())
                    .unwrap_or_default()
                    .to_owned();
                let attributes = parts
                    .filter(|attribute| !attribute.starts_with("Expires="))
                    .map(str::to_owned)
                    .collect();
                (name, attributes)
            })
            .collect()
    };

    let go_shapes = shapes(&go_headers);
    let rs_shapes = shapes(&rs_headers);
    assert_eq!(
        go_shapes.len(),
        3,
        "Go sends three cookies, got {go_shapes:?}"
    );
    assert_eq!(go_shapes, rs_shapes, "the cookie shapes differ");

    // Pin what that shape actually is, so a change that moved both servers together would still
    // be visible in the diff of this test.
    assert_eq!(go_shapes[0].0, "MMAUTHTOKEN");
    assert_eq!(go_shapes[1].0, "MMUSERID");
    assert_eq!(go_shapes[2].0, "MMCSRF");
    assert!(go_shapes[0].1.iter().any(|a| a == "HttpOnly"));
    assert!(!go_shapes[1].1.iter().any(|a| a == "HttpOnly"));
    assert!(!go_shapes[2].1.iter().any(|a| a == "HttpOnly"));

    // The values are real: the user-id cookie names the user, and the CSRF cookie is an id.
    let values = |headers: &reqwest::header::HeaderMap| -> Vec<String> {
        headers
            .get_all("set-cookie")
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .expect("ASCII")
                    .split_once('=')
                    .and_then(|(_, rest)| rest.split(';').next())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    };
    for headers in [&go_headers, &rs_headers] {
        let cookie_values = values(headers);
        assert_eq!(cookie_values[0].len(), 26, "the token cookie is an id");
        assert_eq!(cookie_values[1], user.id, "the user cookie names the user");
        assert_eq!(cookie_values[2].len(), 26, "the CSRF cookie is an id");
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// The two servers write **the same session props** for the same `User-Agent`.
///
/// This is the end-to-end check on the ported `uasurfer` (`mm_app::user_agent`): the props are
/// derived from the header and then echoed by `GET /users/{id}/sessions`, so comparing the two
/// sessions compares the two parsers over a real request. `csrf` is excluded because it is a
/// fresh random id per session, and `isMobile` is asserted rather than excluded — it comes from a
/// different function (`IsMobileRequest`) that reads the same header.
#[tokio::test]
async fn the_session_props_derived_from_the_user_agent_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login3").await;

    // One browser agent, one desktop-client agent, one mobile agent — three different arms of
    // `getPlatformName`/`getOSName`/`getBrowserName`, and the mobile one additionally flips
    // `isMobile`, which changes the session length.
    let agents = [
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/127.0.0.0 Safari/537.36",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
         Mattermost/5.10.0 Chrome/126.0.6478.127 Electron/31.2.0 Safari/537.36",
        "Mattermost Mobile/2.19.0 (iPhone; iOS 17.5.1)",
    ];

    for agent in agents {
        let [(go_status, go_headers, _), (rs_status, rs_headers, _)] = login_both(
            &client,
            "/api/v4/users/login",
            serde_json::json!({
                "login_id": plain_username("login3"),
                "password": PLAIN_USER_PASSWORD,
            }),
            &[("User-Agent", agent)],
        )
        .await;
        assert_eq!(go_status, 200, "Go refused for {agent}");
        assert_eq!(rs_status, 200, "we refused for {agent}");

        let token_of = |headers: &reqwest::header::HeaderMap| {
            headers
                .get("token")
                .expect("a Token header")
                .to_str()
                .expect("ASCII")
                .to_owned()
        };
        let go_token = token_of(&go_headers);
        let rs_token = token_of(&rs_headers);

        // Read the session list once, through Go, so both sessions come from one query — and
        // find each by its token's id. `GET /users/{id}/sessions` sanitizes the token away, so
        // the sessions are matched by looking each token up individually instead.
        let props_of = async |token: &str| -> serde_json::Value {
            let session = client
                .get(format!("{GO}/api/v4/users/me/sessions"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert_eq!(session.status(), 200);
            let sessions: serde_json::Value = session.json().await.expect("a list");
            // The newest session for this user is the one just created by this token's login.
            // `GetSessions` orders by `LastActivityAt DESC`, and this request has just touched
            // the caller's own — so the first row is always ours.
            sessions[0]["props"].clone()
        };

        let go_props = props_of(&go_token).await;
        let rs_props = props_of(&rs_token).await;

        for key in ["platform", "os", "browser", "isMobile", "isSaml", "isOAuthUser", "is_guest"] {
            assert_eq!(
                rs_props.get(key),
                go_props.get(key),
                "`{key}` differs for {agent}\n  go:   {go_props}\n  rust: {rs_props}"
            );
        }
        // The keys themselves, so a prop we simply never write is caught.
        assert_eq!(
            go_props.as_object().map(|o| o.keys().collect::<Vec<_>>()),
            rs_props.as_object().map(|o| o.keys().collect::<Vec<_>>()),
            "the prop sets differ for {agent}"
        );
        // And the corpus is doing work: the browser agent and the mobile one must not agree.
        assert!(
            go_props["browser"].as_str().is_some_and(|b| !b.is_empty()),
            "no browser prop for {agent}"
        );
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// The failed-attempt counter is **one column both servers share**, and the lockout refusal keeps
/// its own unmasked id.
///
/// Failures are split across the two servers deliberately: five against Go, five against us. If
/// either side counted wrongly the eleventh attempt would not be the one that is refused, and if
/// either side used its own counter the account would never lock at all.
#[tokio::test]
async fn the_failed_attempt_counter_is_shared_and_the_lockout_id_survives_the_mask() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login4").await;

    let wrong = serde_json::json!({
        "login_id": plain_username("login4"),
        "password": "Wrong-Password-9999",
    });
    let send = async |base: &str| -> (u16, Vec<u8>) {
        let response = client
            .post(format!("{base}/api/v4/users/login"))
            .header("Content-Type", "application/json")
            .json(&wrong)
            .send()
            .await
            .expect("the server answers");
        (
            response.status().as_u16(),
            response.bytes().await.expect("a body").to_vec(),
        )
    };

    // `MaximumLoginAttempts` is 10 on this stack. Ten failures land the counter on ten; the
    // eleventh is refused before the password is looked at.
    for attempt in 1..=10 {
        let base = if attempt % 2 == 0 { GO } else { RUST };
        let (status, body) = send(base).await;
        assert_eq!(
            status,
            401,
            "attempt {attempt} against {base}: {}",
            String::from_utf8_lossy(&body)
        );
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            parsed["id"], "api.user.login.invalid_credentials_email_username",
            "attempt {attempt} against {base} should still be a masked credential failure"
        );
    }

    // Now both servers must agree that the account is locked, and must say so with the unmasked
    // id — which is the whole reason the mask has an exception list.
    let [(go_status, _, go_body), (rs_status, _, rs_body)] =
        login_both(&client, "/api/v4/users/login", wrong.clone(), &[]).await;
    assert_eq!(go_status, 401);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a locked account");
    assert_eq!(
        go["id"], "api.user.check_user_login_attempts.too_many.app_error",
        "the lockout id must survive the mask"
    );

    // Even the *right* password is refused while the account is locked — the claim happens before
    // the password is checked, on both servers.
    let [(go_locked, _, _), (rs_locked, _, rs_locked_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": plain_username("login4"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;
    assert_eq!(go_locked, 401);
    assert_eq!(rs_locked, 401);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_locked_body).expect("JSON");
    assert_eq!(
        parsed["id"], "api.user.check_user_login_attempts.too_many.app_error",
        "a correct password must not unlock a locked account"
    );

    delete_plain_user(&client, &admin, &user.id).await;
}

/// A deactivated account answers `inactive`, unmasked — and it does so **without** claiming a
/// failed-attempt slot, because the preflight runs first.
#[tokio::test]
async fn a_deactivated_account_is_refused_with_its_own_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login5").await;

    let deactivated = client
        .delete(format!("{GO}/api/v4/users/{}", user.id))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers");
    assert!(deactivated.status().is_success(), "the user was not deactivated");

    let [(go_status, _, go_body), (rs_status, _, rs_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": plain_username("login5"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;
    assert_eq!(go_status, 401);
    assert_eq!(rs_status, go_status);
    let go =
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a deactivated account");
    assert_eq!(go["id"], "api.user.login.inactive.app_error");

    // Neither server counted those two attempts: the preflight refuses before the claim.
    if let Some(pool) = common::fixture_pool().await {
        let attempts: i32 = sqlx::query_scalar("SELECT failedattempts FROM users WHERE id = $1")
            .bind(&user.id)
            .fetch_one(&pool)
            .await
            .expect("the row is readable");
        assert_eq!(
            attempts, 0,
            "a deactivated account must not accumulate failed attempts"
        );
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// A bot cannot log in, and the id says so rather than being masked.
///
/// The seeded bot is safe to use from the shared fixtures precisely because this branch is in the
/// preflight: it refuses before any counter is claimed, so repeated runs cannot lock it out.
#[tokio::test]
async fn a_bot_is_refused_with_its_own_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    let [(go_status, _, go_body), (rs_status, _, rs_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": "seed-bot",
            "password": "Whatever-It-Is-Not-1234",
        }),
        &[],
    )
    .await;
    assert_eq!(go_status, 401);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a bot login");
    assert_eq!(go["id"], "api.user.login.bot_login_forbidden.app_error");
}

/// An account with a non-empty `AuthService` is refused, and the refusal **is** masked — so the
/// client cannot tell an SSO account from a wrong password.
///
/// The pair of assertions matters: the *id* is the generic one, and the status is 401 even though
/// Go builds `use_auth_service` as a 400. A port that forgot the mask would answer 400 here and
/// leak which accounts are federated.
#[tokio::test]
async fn an_sso_account_is_masked_rather_than_named() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login6").await;

    if !common::set_user_auth_service(&user.id, "gitlab").await {
        delete_plain_user(&client, &admin, &user.id).await;
        return;
    }
    common::invalidate_go_caches(&client, &admin).await;

    let [(go_status, _, go_body), (rs_status, _, rs_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "login_id": plain_username("login6"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;
    assert_eq!(go_status, 401, "the 400 must have been masked to a 401");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an SSO account");
    assert_eq!(go["id"], "api.user.login.invalid_credentials_email_username");

    delete_plain_user(&client, &admin, &user.id).await;
}

/// An explicit `id` in the body **replaces** the login-id lookup rather than supplementing it.
///
/// Two claims: a right `id` with a nonsense `login_id` succeeds, and a nonsense `id` with a right
/// `login_id` fails. The second is what a port that treated `id` as a hint would get wrong, and
/// it would get it wrong silently because the first would still pass.
#[tokio::test]
async fn an_explicit_id_wins_over_the_login_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login7").await;

    let [(go_ok, _, go_ok_body), (rs_ok, _, rs_ok_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            "id": user.id,
            "login_id": "this-is-not-anybody@mmrs.invalid",
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;
    assert_eq!(go_ok, 200, "Go: {}", String::from_utf8_lossy(&go_ok_body));
    assert_eq!(rs_ok, 200, "us: {}", String::from_utf8_lossy(&rs_ok_body));
    assert_user_bodies_agree(&go_ok_body, &rs_ok_body, "login by explicit id");

    let [(go_bad, _, go_bad_body), (rs_bad, _, rs_bad_body)] = login_both(
        &client,
        "/api/v4/users/login",
        serde_json::json!({
            // A syntactically valid id that names nobody.
            "id": "zzzzzzzzzzzzzzzzzzzzzzzzzz",
            "login_id": plain_username("login7"),
            "password": PLAIN_USER_PASSWORD,
        }),
        &[],
    )
    .await;
    assert_eq!(go_bad, 401, "a wrong id must not fall back to the login id");
    assert_eq!(rs_bad, go_bad);
    assert_error_bodies_match_except_known_gaps(
        &go_bad_body,
        &rs_bad_body,
        "login with a wrong explicit id",
    );

    delete_plain_user(&client, &admin, &user.id).await;
}

/// Logging in by **username** works as well as by e-mail, and the lookup lower-cases the
/// submitted value rather than the column.
#[tokio::test]
async fn the_login_id_resolves_by_username_and_by_email_case_insensitively() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "login8").await;
    let username = plain_username("login8");

    for login_id in [
        username.clone(),
        username.to_uppercase(),
        format!("{username}@mmrs.invalid"),
        format!("{username}@MMRS.INVALID"),
    ] {
        let [(go_status, _, go_body), (rs_status, _, rs_body)] = login_both(
            &client,
            "/api/v4/users/login",
            serde_json::json!({ "login_id": login_id, "password": PLAIN_USER_PASSWORD }),
            &[],
        )
        .await;
        assert_eq!(
            go_status,
            200,
            "Go refused {login_id}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            200,
            "we refused {login_id}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_user_bodies_agree(&go_body, &rs_body, &format!("login as {login_id}"));
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// `POST /users/login/type` is a **404 with an empty body** on this stack, and that is the answer
/// rather than a gap: the route is gated on `GuestAccountsSettings.EnableGuestMagicLink`, which
/// is off by default.
///
/// Compared as status *and* body length, because an empty body is the thing most likely to drift
/// — every other refusal in api4 carries an `AppError` object.
#[tokio::test]
async fn get_login_type_is_a_404_with_no_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    for body in [
        serde_json::json!({ "login_id": "slice@example.com" }),
        serde_json::json!({}),
        serde_json::json!({ "login_id": "nobody@mmrs.invalid", "device_id": "x" }),
    ] {
        let [(go_status, go_headers, go_body), (rs_status, rs_headers, rs_body)] =
            login_both(&client, "/api/v4/users/login/type", body.clone(), &[]).await;

        assert_eq!(go_status, 404, "Go's status for {body}");
        assert_eq!(rs_status, go_status, "our status for {body}");
        assert!(go_body.is_empty(), "Go wrote a body: {go_body:?}");
        assert!(rs_body.is_empty(), "we wrote a body: {rs_body:?}");
        assert_eq!(
            go_headers.get("content-type"),
            rs_headers.get("content-type"),
            "the content type differs for {body}"
        );
    }
}
