//! Cross-server parity for **MFA enrolment and the MFA login** (D-500, D-1210): the secret one
//! server mints must work on the other, the replay list one server writes must bind the other, and
//! every refusal must be the same.
//!
//! ```sh
//! MMRS_LICENSED_VARIANT=mfa scripts/go-licensed.sh start
//! scripts/parity.sh --test parity mfa_enrolment
//! ```
//!
//! Runs on the licensed **MFA** pair, the only servers with `EnableMultifactorAuthentication` on.
//! Enforcement is on there too, which does not reach these routes: the two set-up routes are
//! `APISessionRequiredMfa`, and `login` takes no session.
//!
//! The TOTP codes are computed with `mm_app::otp::compute_code`, which is itself checked against
//! `dgoogauth` row by row (`fixtures/behaviour_mfa.json`). A login uses the code for the **next**
//! step: the activation spent the current one, and the next is inside the ±1 window whichever side
//! of a step boundary the test lands on.

use crate::common;

use common::{
    PLAIN_USER_PASSWORD, PlainUser, client, create_plain_user, create_team, go_minted_token,
    licensed_mfa, plain_username, request_raw, stack_enabled,
};

/// `(status, error id)` — the id empty for a success.
fn outcome(status: u16, body: &[u8]) -> (u16, String) {
    let id = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned))
        .filter(|_| status >= 400)
        .unwrap_or_default();
    (status, id)
}

fn code_at(secret: &str, offset: i64) -> String {
    let code = mm_app::otp::compute_code(secret, mm_app::otp::current_step() + offset);
    format!("{code:06}")
}

async fn login(
    http: &reqwest::Client,
    base: &str,
    tag: &str,
    token: Option<&str>,
) -> ((u16, String), Option<String>) {
    let mut body = serde_json::json!({
        "login_id": plain_username(tag),
        "password": PLAIN_USER_PASSWORD,
    });
    if let Some(token) = token {
        body["token"] = serde_json::Value::String(token.to_owned());
    }
    let (status, bytes, served) = request_raw(
        http,
        base,
        reqwest::Method::POST,
        None,
        "/api/v4/users/login",
        Some(body.to_string().as_bytes()),
    )
    .await;
    (outcome(status, &bytes), served)
}

/// `POST /mfa/generate` as the user: the secret, after checking the answer's shape and headers.
async fn generate(http: &reqwest::Client, base: &str, user: &PlainUser) -> String {
    let response = http
        .post(format!("{base}/api/v4/users/{}/mfa/generate", user.id))
        .bearer_auth(&user.token)
        .send()
        .await
        .expect("the server answers");
    assert_eq!(response.status(), 200, "{base}: generate");
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        header("cache-control").as_deref(),
        Some("no-cache"),
        "{base}"
    );
    assert_eq!(header("pragma").as_deref(), Some("no-cache"), "{base}");
    assert_eq!(header("expires").as_deref(), Some("0"), "{base}");
    let body = response.bytes().await.expect("a body");
    assert_eq!(
        body.last(),
        Some(&b'\n'),
        "{base}: json.NewEncoder's newline"
    );
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let object = value.as_object().expect("an object");
    assert_eq!(object.len(), 2, "{base}: secret and qr_code only: {value}");
    let secret = value["secret"].as_str().expect("a secret").to_owned();
    assert_eq!(secret.len(), 32, "{base}: 160 bits of base32");
    assert!(mm_app::otp::base32_decode(&secret).is_some_and(|b| b.len() == 20));
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        value["qr_code"].as_str().expect("a QR code"),
    )
    .expect("base64");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "{base}: a PNG");
    secret
}

async fn put_mfa(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    body: &str,
) -> ((u16, String), Option<String>) {
    let (status, bytes, served) = request_raw(
        http,
        base,
        reqwest::Method::PUT,
        Some(token),
        &format!("/api/v4/users/{user_id}/mfa"),
        Some(body.as_bytes()),
    )
    .await;
    (outcome(status, &bytes), served)
}

/// Enrol on one server, log in on the other, both ways round; the replay list one server writes
/// refuses the same code on the other; every refusal is the same on both.
///
/// **Ordered around Go's user cache.** Go memoises the user row, and on this pair the purge this
/// server sends after a write is refused by MFA enforcement (D-1260). So no write is made here to
/// a user Go has already read, and the deactivation is Go's.
#[tokio::test]
async fn a_secret_minted_on_one_server_logs_in_on_the_other() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let pair = licensed_mfa().await;
    let team = create_team(&http, &admin, "mfaen").await;

    // Enrolled through `enrol_on`, logged in through `login_on`.
    for (tag, enrol_on, login_on) in [
        ("mfaenr", pair.rust.as_str(), pair.go.as_str()),
        ("mfaeng", pair.go.as_str(), pair.rust.as_str()),
    ] {
        let user = create_plain_user(&http, &admin, &team, tag).await;
        let secret = generate(&http, enrol_on, &user).await;

        // Activation refusals, the same on both servers: a code that is not six digits is
        // Go's 500, a wrong one the 401, a missing one the 400. Only where Go enrols: a Go read
        // made before a write here would leave Go's cached user stale (D-1260).
        let refusals: &[(&str, (u16, &str))] = if enrol_on == pair.go {
            &[
                (
                    r#"{"activate":true,"code":"abc"}"#,
                    (500, "mfa.activate.app_error"),
                ),
                (
                    r#"{"activate":true,"code":"000000"}"#,
                    (401, "mfa.activate.bad_token.app_error"),
                ),
                (
                    r#"{"activate":true}"#,
                    (400, "api.context.invalid_body_param.app_error"),
                ),
            ]
        } else {
            &[]
        };
        for &(body, want) in refusals {
            if body.contains("000000") && code_at(&secret, 0) == "000000" {
                // One chance in a million: the wrong code is right.
                continue;
            }
            for base in [pair.go.as_str(), pair.rust.as_str()] {
                let (got, served) = put_mfa(&http, base, &user.token, &user.id, body).await;
                assert_eq!(got, (want.0, want.1.to_owned()), "{tag} {base} {body}");
                if base == pair.rust {
                    assert_eq!(served.as_deref(), Some("rust"), "{body}");
                }
            }
        }

        let code = code_at(&secret, 0);
        let (got, served) = put_mfa(
            &http,
            enrol_on,
            &user.token,
            &user.id,
            &format!(r#"{{"activate":true,"code":"{code}"}}"#),
        )
        .await;
        assert_eq!(got, (200, String::new()), "{tag}: activation on {enrol_on}");
        if enrol_on == pair.rust {
            assert_eq!(served.as_deref(), Some("rust"));
        }

        // The same code again is a replay on either server.
        for base in [pair.go.as_str(), pair.rust.as_str()] {
            let (got, _) = put_mfa(
                &http,
                base,
                &user.token,
                &user.id,
                &format!(r#"{{"activate":true,"code":"{code}"}}"#),
            )
            .await;
            assert_eq!(
                got,
                (401, "mfa.activate.bad_token.app_error".to_owned()),
                "{tag}: {base} replays the activation code"
            );
        }

        // Login refusals, both servers: no token is the parse 400 (and refunds the slot), a
        // wrong one the 401, a short one the 400, the spent code the 401.
        for token in [None, Some("000000"), Some("12345"), Some(code.as_str())] {
            let (go, _) = login(&http, &pair.go, tag, token).await;
            let (rust, served) = login(&http, &pair.rust, tag, token).await;
            assert_eq!(rust, go, "{tag}: login with {token:?}");
            assert_eq!(
                served.as_deref(),
                Some("rust"),
                "{tag}: {token:?} is served"
            );
            assert_ne!(go.0, 200, "{tag}: {token:?}");
        }

        // The next step's code logs in on the other server — and is then spent on both.
        let next = code_at(&secret, 1);
        let (got, served) = login(&http, login_on, tag, Some(&next)).await;
        assert_eq!(got, (200, String::new()), "{tag}: login on {login_on}");
        if login_on == pair.rust {
            assert_eq!(served.as_deref(), Some("rust"));
        }
        for base in [pair.go.as_str(), pair.rust.as_str()] {
            let (got, _) = login(&http, base, tag, Some(&next)).await;
            assert_eq!(
                got,
                (401, "api.user.check_user_mfa.bad_code.app_error".to_owned()),
                "{tag}: {base} refuses the spent login code"
            );
        }

        // Deactivated on Go, whose cache must see it (D-1260); the served deactivation is
        // `parity::user_auth::deactivating_mfa_is_served_and_answers_ok`. No code needed anywhere.
        let (got, _) = put_mfa(
            &http,
            &pair.go,
            &user.token,
            &user.id,
            r#"{"activate":false}"#,
        )
        .await;
        assert_eq!(got, (200, String::new()), "{tag}: deactivation");
        for base in [pair.go.as_str(), pair.rust.as_str()] {
            let (got, _) = login(&http, base, tag, None).await;
            assert_eq!(
                got,
                (200, String::new()),
                "{tag}: {base} after deactivation"
            );
        }
        common::delete_plain_user(&http, &admin, &user.id).await;
    }
}

/// An administrator changing somebody else's MFA must satisfy enforcement themselves: the stack's
/// administrator has none, so `MFARequired` refuses — before the body is read, so even a malformed
/// one gets the enforcement error. Their own account is exempt from the check.
#[tokio::test]
async fn an_unenrolled_admin_cannot_change_somebody_elses_mfa() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let pair = licensed_mfa().await;
    let team = create_team(&http, &admin, "mfaad").await;
    let user = create_plain_user(&http, &admin, &team, "mfaadm").await;
    for body in [r#"{"activate":false}"#, "not json"] {
        let (go, _) = put_mfa(&http, &pair.go, &admin, &user.id, body).await;
        let (rust, served) = put_mfa(&http, &pair.rust, &admin, &user.id, body).await;
        assert_eq!(rust, go, "{body}");
        assert_eq!(go.0, 403, "{body}: {go:?}");
        assert_eq!(served.as_deref(), Some("rust"));
    }
    common::delete_plain_user(&http, &admin, &user.id).await;
}
