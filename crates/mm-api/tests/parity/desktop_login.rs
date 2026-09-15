//! Cross-server parity for `POST /api/v4/users/login/desktop_token`, the desktop app's half of
//! an SSO login.
//!
//! ```sh
//! scripts/parity.sh --test parity desktop_login
//! ```
//!
//! No SSO provider is configured on the stack, so the `DesktopTokens` rows the route trades are
//! planted straight into the table — which is all the browser-side completion would have done —
//! and the SSO accounts are plain users whose `AuthService` is rewritten in place. Go's route
//! carries its own rate limit of 2/s with a burst of 1, so every request to Go here is spaced
//! and serialised; ours has no limit ([D-430]).

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fixture_pool, go_minted_token, stack_enabled,
};

/// `DesktopTokenTTL` is three minutes; a token this much older than now is expired.
const EXPIRED_AGE_SECONDS: i64 = 3 * 60 + 5;

/// Go's `RateLimitedHandler(…, PerSec: 2, MaxBurst: 1)` on this one route: one request every
/// 500ms per remote address. Every call to Go goes through this lock and this wait.
static GO_THROTTLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A fresh 64-character token, the width of the column.
fn new_token() -> String {
    let id = || mm_model::utils::new_id();
    format!("{}{}{}", id(), id(), id())[..64].to_owned()
}

async fn plant_token(pool: &sqlx::PgPool, token: &str, create_at: i64, user_id: &str) {
    sqlx::query("INSERT INTO desktoptokens (token, createat, userid) VALUES ($1, $2, $3)")
        .bind(token)
        .bind(create_at)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("the token row is planted");
}

async fn token_rows_for(pool: &sqlx::PgPool, user_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM desktoptokens WHERE userid = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The one thing an SSO account has that a plain one does not: `AuthService` naming the
/// provider and `AuthData` naming the subject. Written through Go's own `PUT /users/{id}/auth`
/// rather than SQL, because Go caches user rows and a row rewritten underneath it would leave
/// Go answering for the plain account while we answered for the SSO one. The call also revokes
/// the account's sessions, so anything read off the plain user's token is read first.
async fn make_sso(client: &reqwest::Client, admin: &str, user_id: &str, service: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/auth"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "auth_data": format!("subject-{user_id}"),
            "auth_service": service,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "the account becomes SSO: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    body: &str,
) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let _guard = if base == GO {
        let guard = GO_THROTTLE.lock().await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        Some(guard)
    } else {
        None
    };
    let response = client
        .post(format!("{base}/api/v4/users/login/desktop_token"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    (
        status,
        headers,
        response.bytes().await.expect("body reads").to_vec(),
    )
}

fn served_here(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust")
}

fn id_of(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The same refusal from both: status, id, and the body around the known gaps.
async fn both_refuse(client: &reqwest::Client, body: &str, status: u16, id: &str) {
    let (go_status, _, go) = post(client, GO, body).await;
    let (rs_status, rs_headers, rs) = post(client, RUST, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served_here(&rs_headers), "{body}: served here");
    assert_eq!(id_of(&go), id, "{body}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/users/login/desktop_token");
}

struct SessionRow {
    length_seconds: i64,
    props: serde_json::Value,
    device_id: String,
}

async fn session_by_token(pool: &sqlx::PgPool, token: &str) -> SessionRow {
    let (create_at, expires_at, props, device_id): (i64, i64, serde_json::Value, String) =
        sqlx::query_as(
            "SELECT createat, expiresat, props, deviceid FROM sessions WHERE token = $1",
        )
        .bind(token)
        .fetch_one(pool)
        .await
        .expect("the session row exists");
    SessionRow {
        // `SetSessionExpireInHours` adds hours to a fresh clock read, not to `CreateAt`, so the
        // difference is a few milliseconds off a whole number of seconds: rounded, not truncated.
        length_seconds: (expires_at - create_at + 500) / 1000,
        props,
        device_id,
    }
}

/// Two servers' session lengths, equal to within a few seconds.
///
/// Both stamp `ExpiresAt` from a clock read taken before the session is saved and `CreateAt` is
/// set (`SetSessionExpireInHours`, then the store's `PreSave`), so `ExpiresAt - CreateAt` is the
/// configured length minus however long the login took between the two reads. Rounding absorbs
/// that on an idle stack; under a full run one login took long enough that ours read 2,591,998
/// seconds against Go's 2,592,000 while passing alone twice (2026-09-15). A real difference — the
/// SSO length against the web length — is hours, not seconds.
fn assert_same_length(go: i64, ours: i64, context: &str) {
    assert!((go - ours).abs() <= 5, "{context}: Go {go}s, ours {ours}s");
}

/// The fields of the success body that cannot agree across two sequential logins: Go's login
/// stamps `LastLogin` after reading the row it serialises, so ours reads Go's stamp and Go read
/// the one before it. `update_at` moves with it.
const VOLATILE: &[&str] = &["update_at", "last_login"];

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
        "{context}: the same keys.\n  go:   {go}\n  rust: {rs}"
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
    assert_eq!(
        go_body.last().copied(),
        Some(b'\n'),
        "{context}: Go's newline"
    );
    assert_eq!(
        rs_body.last().copied(),
        Some(b'\n'),
        "{context}: our newline"
    );
}

/// Both cookie lists, name and attributes, `Expires=` aside.
fn cookie_shapes(headers: &reqwest::header::HeaderMap) -> Vec<(String, Vec<String>)> {
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
}

/// An unknown token, an empty body, and a token past its three minutes: the 401 `validate.invalid`
/// on both — and the expired row is gone afterwards on both, since a miss deletes what it missed.
#[tokio::test]
async fn a_missing_or_expired_token_is_the_401_and_is_deleted() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "dtok").await;
    let user = create_plain_user(&client, &admin, &team, "dtokx").await;

    for body in ["{}", "", "[]", r#"{"token":""}"#] {
        both_refuse(&client, body, 401, "app.desktop_token.validate.invalid").await;
    }
    let unknown = new_token();
    both_refuse(
        &client,
        &format!(r#"{{"token":"{unknown}"}}"#),
        401,
        "app.desktop_token.validate.invalid",
    )
    .await;

    for base in [GO, RUST] {
        let expired = new_token();
        plant_token(
            &pool,
            &expired,
            now_seconds() - EXPIRED_AGE_SECONDS,
            &user.id,
        )
        .await;
        assert_eq!(token_rows_for(&pool, &user.id).await, 1);
        let (status, _, body) = post(&client, base, &format!(r#"{{"token":"{expired}"}}"#)).await;
        assert_eq!(status, 401, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(id_of(&body), "app.desktop_token.validate.invalid");
        assert_eq!(
            token_rows_for(&pool, &user.id).await,
            0,
            "{base}: the expired row is deleted by the miss"
        );
    }
}

/// A live token for an email account: the 401 `not_oauth_or_saml_user`, after the validation
/// has already consumed every token the account had — the refusal is not free.
#[tokio::test]
async fn an_email_account_is_refused_after_its_tokens_are_consumed() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "dtoe").await;
    let user = create_plain_user(&client, &admin, &team, "dtoke").await;

    for base in [GO, RUST] {
        let token = new_token();
        let other = new_token();
        plant_token(&pool, &token, now_seconds(), &user.id).await;
        plant_token(&pool, &other, now_seconds() - 10, &user.id).await;
        let (status, _, body) = post(&client, base, &format!(r#"{{"token":"{token}"}}"#)).await;
        assert_eq!(status, 401, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(
            id_of(&body),
            "api.user.login_with_desktop_token.not_oauth_or_saml_user.app_error"
        );
        assert_eq!(
            token_rows_for(&pool, &user.id).await,
            0,
            "{base}: both tokens are gone, the unused one included"
        );
    }

    // And the two refusals agree on the wire.
    let (token_go, token_rs) = (new_token(), new_token());
    plant_token(&pool, &token_go, now_seconds(), &user.id).await;
    let (_, _, go) = post(&client, GO, &format!(r#"{{"token":"{token_go}"}}"#)).await;
    plant_token(&pool, &token_rs, now_seconds(), &user.id).await;
    let (_, _, rs) = post(&client, RUST, &format!(r#"{{"token":"{token_rs}"}}"#)).await;
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/users/login/desktop_token");
}

/// An OAuth account and a SAML one: the unsanitised user with a `Token` header and the three
/// cookies, a session of the **SSO** length carrying the right `isOAuthUser`/`isSaml` props, and
/// a token that works once.
#[tokio::test]
async fn an_sso_account_gets_a_session_of_the_sso_length_and_the_token_is_spent() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "dtos").await;

    for (tag, service, oauth, saml) in [
        ("dtokg", "gitlab", "true", "false"),
        ("dtoks", "saml", "false", "true"),
    ] {
        let user = create_plain_user(&client, &admin, &team, tag).await;
        make_sso(&client, &admin, &user.id, service).await;

        let token_go = new_token();
        plant_token(&pool, &token_go, now_seconds(), &user.id).await;
        let (go_status, go_headers, go_body) =
            post(&client, GO, &format!(r#"{{"token":"{token_go}"}}"#)).await;
        assert_eq!(
            go_status,
            200,
            "Go {service}: {}",
            String::from_utf8_lossy(&go_body)
        );

        let token_rs = new_token();
        plant_token(&pool, &token_rs, now_seconds(), &user.id).await;
        let (rs_status, rs_headers, rs_body) =
            post(&client, RUST, &format!(r#"{{"token":"{token_rs}"}}"#)).await;
        assert_eq!(
            rs_status,
            200,
            "Rust {service}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert!(served_here(&rs_headers), "{service}: served here");

        assert_user_bodies_agree(&go_body, &rs_body, service);
        let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
        assert_eq!(parsed["auth_service"], service);
        assert_eq!(
            parsed["auth_data"],
            format!("subject-{}", user.id),
            "{service}: the row is written unsanitised, auth_data included"
        );
        assert!(
            parsed.get("password").is_none(),
            "an SSO row has no password"
        );

        // The cookies, unconditionally — no `X-Requested-With` was sent.
        let go_shapes = cookie_shapes(&go_headers);
        assert_eq!(go_shapes.len(), 3, "Go sends three cookies: {go_shapes:?}");
        assert_eq!(
            go_shapes,
            cookie_shapes(&rs_headers),
            "{service}: cookie shapes"
        );

        // The sessions: both the SSO length, both with the SSO props, no device.
        let go_token = go_headers
            .get("Token")
            .expect("Go's Token header")
            .to_str()
            .unwrap();
        let rs_token = rs_headers
            .get("Token")
            .expect("our Token header")
            .to_str()
            .unwrap();
        let go_session = session_by_token(&pool, go_token).await;
        let rs_session = session_by_token(&pool, rs_token).await;
        assert_same_length(
            go_session.length_seconds,
            rs_session.length_seconds,
            &format!("{service}: the session length"),
        );
        assert_eq!(
            go_session.props["isOAuthUser"], oauth,
            "{service}: Go's isOAuthUser"
        );
        assert_eq!(go_session.props["isSaml"], saml, "{service}: Go's isSaml");
        let without_csrf = |props: &serde_json::Value| {
            let mut props = props.clone();
            props.as_object_mut().map(|o| o.remove("csrf"));
            props
        };
        assert_eq!(
            without_csrf(&go_session.props),
            without_csrf(&rs_session.props),
            "{service}: the session props, the CSRF token aside"
        );
        assert_eq!(rs_session.device_id, "");

        // Spent: the same token again is the 401 on both.
        let (status, _, body) = post(&client, GO, &format!(r#"{{"token":"{token_go}"}}"#)).await;
        assert_eq!(
            status,
            401,
            "Go: a token works once: {}",
            String::from_utf8_lossy(&body)
        );
        let (status, _, body) = post(&client, RUST, &format!(r#"{{"token":"{token_rs}"}}"#)).await;
        assert_eq!(
            status,
            401,
            "Rust: a token works once: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(id_of(&body), "app.desktop_token.validate.invalid");
    }
}

/// The SSO length is its own setting: a plain `login` for the same kind of account measures
/// the web length, and the two differ on the stack — so a port that took the web arm would be
/// caught here rather than passing by coincidence.
#[tokio::test]
async fn the_session_is_not_the_web_length() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "dtol").await;
    let user = create_plain_user(&client, &admin, &team, "dtokl").await;

    // The web length, from a password login before the account turns SSO.
    let web: (i64, i64) =
        sqlx::query_as("SELECT createat, expiresat FROM sessions WHERE token = $1")
            .bind(&user.token)
            .fetch_one(&pool)
            .await
            .expect("the plain user's session");
    let web_length = (web.1 - web.0) / 1000;

    make_sso(&client, &admin, &user.id, "google").await;
    let token = new_token();
    plant_token(&pool, &token, now_seconds(), &user.id).await;
    let (status, headers, body) = post(&client, RUST, &format!(r#"{{"token":"{token}"}}"#)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let sso = session_by_token(&pool, headers.get("Token").unwrap().to_str().unwrap()).await;
    assert_ne!(
        sso.length_seconds, web_length,
        "the stack's SSO and web lengths must differ for this suite to measure anything"
    );

    // A device id makes it mobile instead, on both — and a malformed one is the 400 before
    // any session exists, though the token is already spent.
    let mobile_go = new_token();
    plant_token(&pool, &mobile_go, now_seconds(), &user.id).await;
    let (status, go_headers, body) = post(
        &client,
        GO,
        &format!(
            r#"{{"token":"{mobile_go}","device_id":"apple_rn:{}"}}"#,
            mm_model::utils::new_id()
        ),
    )
    .await;
    assert_eq!(status, 200, "Go mobile: {}", String::from_utf8_lossy(&body));
    let mobile_rs = new_token();
    plant_token(&pool, &mobile_rs, now_seconds(), &user.id).await;
    let (status, rs_headers, body) = post(
        &client,
        RUST,
        &format!(
            r#"{{"token":"{mobile_rs}","device_id":"apple_rn:{}"}}"#,
            mm_model::utils::new_id()
        ),
    )
    .await;
    assert_eq!(
        status,
        200,
        "Rust mobile: {}",
        String::from_utf8_lossy(&body)
    );
    let go_mobile =
        session_by_token(&pool, go_headers.get("Token").unwrap().to_str().unwrap()).await;
    let rs_mobile =
        session_by_token(&pool, rs_headers.get("Token").unwrap().to_str().unwrap()).await;
    assert_same_length(
        go_mobile.length_seconds,
        rs_mobile.length_seconds,
        "the mobile session length",
    );
    assert_eq!(go_mobile.props["isMobile"], "true");
    assert_eq!(rs_mobile.props["isMobile"], "true");
    assert!(rs_mobile.device_id.starts_with("apple_rn:"));

    let bad = new_token();
    plant_token(&pool, &bad, now_seconds(), &user.id).await;
    let (_, _, go) = post(
        &client,
        GO,
        &format!(r#"{{"token":"{bad}","device_id":"nope"}}"#),
    )
    .await;
    let bad = new_token();
    plant_token(&pool, &bad, now_seconds(), &user.id).await;
    let (status, _, rs) = post(
        &client,
        RUST,
        &format!(r#"{{"token":"{bad}","device_id":"nope"}}"#),
    )
    .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&rs));
    assert_eq!(
        id_of(&go),
        "api.user.attach_device_id.invalid_device_id.app_error"
    );
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/users/login/desktop_token");
    assert_eq!(
        token_rows_for(&pool, &user.id).await,
        0,
        "spent before the 400"
    );
}
