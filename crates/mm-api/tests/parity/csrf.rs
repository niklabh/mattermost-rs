//! Cross-server parity for `checkCSRFToken` (web/handlers.go:508) — the check `ServeHTTP` applies to
//! a **cookie**-authenticated request that is not a `GET`.
//!
//! ```sh
//! scripts/parity.sh --test parity csrf
//! ```
//!
//! Every other suite authenticates with a bearer token, which the check never looks at, so until
//! this module no request in the binary could tell a server that checks CSRF from one that does
//! not. The fixture is a browser login: `POST /users/login` with `X-Requested-With` makes Go set
//! `MMAUTHTOKEN` and `MMCSRF`, and each request below replays the first as a `Cookie` and the second
//! (or not) as `X-CSRF-Token`.
//!
//! One route per way this server reaches the check — `AuthenticatedSession`, `OptionalSession`,
//! `CsrfGuard` on a handler that takes no session, and the `TrustRequester` exemption — because
//! each is a separate call site that a mutation can remove on its own. Every Rust answer is
//! asserted as served here: a forwarded request gets Go's check and would pass every comparison.
//!
//! The strict branch is the one place this suite writes shared state: it patches Go's
//! `ExperimentalStrictCSRFEnforcement` on, compares Go with a second mm-api started after the patch
//! (the main one holds a start-up snapshot of the configuration, D-701), and patches it back off
//! before asserting anything.

use futures_util::FutureExt;

use crate::common;

use common::{
    GO, PLAIN_USER_PASSWORD, RUST, SecondServer, a_team_and_channel_the_user_is_in,
    assert_served_by_rust, client, create_plain_user, delete_plain_user, go_minted_token,
    plain_username, stack_enabled,
};

/// `SecondServer` port for the strict-enforcement mm-api. Unique in the binary —
/// `parity::second_server_ports` checks.
const STRICT_PORT: u16 = 8093;

/// A browser session: the auth cookie and the CSRF token Go handed out with it.
struct BrowserSession {
    token: String,
    csrf: String,
}

/// Log `tag`'s plain user in the way the webapp does, and read the two cookies back.
async fn browser_login(http: &reqwest::Client, tag: &str) -> BrowserSession {
    let response = http
        .post(format!("{GO}/api/v4/users/login"))
        .header("X-Requested-With", "XMLHttpRequest")
        .json(&serde_json::json!({
            "login_id": plain_username(tag),
            "password": PLAIN_USER_PASSWORD,
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "the browser login succeeds");
    let cookie = |name: &str| -> String {
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|raw| {
                raw.split(';')
                    .next()
                    .and_then(|pair| pair.strip_prefix(&format!("{name}=")))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("Go sets {name}"))
    };
    BrowserSession {
        token: cookie("MMAUTHTOKEN"),
        csrf: cookie("MMCSRF"),
    }
}

/// What a response is, for comparison: status, the body with its `request_id` removed, and the
/// `Set-Cookie` values in order.
#[derive(Debug, PartialEq)]
struct Answer {
    status: u16,
    body: serde_json::Value,
    cookies: Vec<String>,
}

async fn answer(response: reqwest::Response) -> Answer {
    let status = response.status().as_u16();
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let bytes = response.bytes().await.expect("body reads");
    let mut body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
    });
    if let Some(object) = body.as_object_mut() {
        object.remove("request_id");
    }
    Answer {
        status,
        body,
        cookies,
    }
}

/// One request shape, sent to Go and to `rust`, both answers compared and the Rust one required to
/// be served. Returns the shared answer so the caller can pin what it is.
async fn both(
    http: &reqwest::Client,
    rust: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
    context: &str,
) -> Answer {
    let send = async |base: &str| {
        let mut request = http
            .request(method.clone(), format!("{base}{path}"))
            .header("Content-Type", "application/json")
            .body(body.to_owned());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.expect("the server answers")
    };
    let go = answer(send(GO).await).await;
    let rs_response = send(rust).await;
    assert_served_by_rust(rs_response.headers(), &format!("{context}: {path}"));
    let rs = answer(rs_response).await;
    let (go, rs) = comparable(go, rs);
    assert_eq!(go, rs, "{context}: {method} {path}");
    go
}

/// Drop `message` from two error bodies when ours is the raw id — D-092, the one key the harness
/// tolerates (see `assert_error_bodies_match_except_known_gaps`). Anything else still compares.
///
/// A **success** body is not compared at all: what the check decides is the status and the
/// cookie, and the handlers' own bodies are pinned by their suites — `POST /users/ids` embeds the
/// user's `update_at`, which Go's post-login goroutines move between the two reads.
fn comparable(mut go: Answer, mut rs: Answer) -> (Answer, Answer) {
    if go.status < 300 && rs.status < 300 {
        go.body = serde_json::Value::Null;
        rs.body = serde_json::Value::Null;
    }
    if rs.body.get("message").is_some() && rs.body.get("message") == rs.body.get("id") {
        for body in [&mut go.body, &mut rs.body] {
            if let Some(object) = body.as_object_mut() {
                object.remove("message");
            }
        }
    }
    (go, rs)
}

const CLEARED: &str = "MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly";

fn assert_csrf_refusal(answer: &Answer, context: &str) {
    assert_eq!(answer.status, 401, "{context}");
    assert_eq!(
        answer.body["id"], "api.context.session_expired.app_error",
        "{context}"
    );
    assert_eq!(answer.cookies, vec![CLEARED.to_owned()], "{context}");
}

#[tokio::test]
async fn a_cookie_write_must_carry_the_sessions_csrf_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team_id, "csrfcookie").await;
    let session = browser_login(&http, "csrfcookie").await;
    let cookie = format!("MMAUTHTOKEN={}", session.token);
    let ids = serde_json::json!([user.id]).to_string();
    let post = reqwest::Method::POST;

    // AuthenticatedSession — `POST /users/ids`.
    let refused = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[("Cookie", &cookie)],
        "no header",
    )
    .await;
    assert_csrf_refusal(&refused, "a cookie write with no CSRF header");

    let refused = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[("Cookie", &cookie), ("X-CSRF-Token", "notthetoken")],
        "wrong token",
    )
    .await;
    assert_csrf_refusal(&refused, "a cookie write with the wrong CSRF token");

    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[("Cookie", &cookie), ("X-CSRF-Token", &session.csrf)],
        "right token",
    )
    .await;
    assert_eq!(passed.status, 200, "the matching token passes");

    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[
            ("Cookie", &cookie),
            ("X-CSRF-Token", "notthetoken"),
            ("X-Requested-With", "XMLHttpRequest"),
        ],
        "legacy header, lenient",
    )
    .await;
    assert_eq!(
        passed.status, 200,
        "X-Requested-With rescues a mismatch while enforcement is lenient"
    );

    // The bearer token is never checked, and neither is a GET with the cookie.
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[("Authorization", &format!("Bearer {}", session.token))],
        "bearer",
    )
    .await;
    assert_eq!(passed.status, 200, "a header token is not CSRF-checked");
    let passed = both(
        &http,
        RUST,
        reqwest::Method::GET,
        "/api/v4/users/me",
        "",
        &[("Cookie", &cookie)],
        "GET",
    )
    .await;
    assert_eq!(passed.status, 200, "a GET is not CSRF-checked");

    // OptionalSession — `POST /logs` is `APIHandler`, and the check still refuses.
    let log = r#"{"level":"DEBUG","message":"mmrs csrf parity"}"#;
    let refused = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/logs",
        log,
        &[("Cookie", &cookie)],
        "optional, no header",
    )
    .await;
    assert_csrf_refusal(&refused, "an optional-session write with no CSRF header");
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/logs",
        log,
        &[("Cookie", &cookie), ("X-CSRF-Token", &session.csrf)],
        "optional, right token",
    )
    .await;
    assert_eq!(passed.status, 200);

    // CsrfGuard — `POST /users/login/type` takes no session at all.
    let login_type = serde_json::json!({ "login_id": plain_username("csrfcookie") }).to_string();
    let refused = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/login/type",
        &login_type,
        &[("Cookie", &cookie)],
        "sessionless, no header",
    )
    .await;
    assert_csrf_refusal(
        &refused,
        "a sessionless handler with a live cookie and no CSRF header",
    );
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/login/type",
        &login_type,
        &[("Cookie", &cookie), ("X-CSRF-Token", &session.csrf)],
        "sessionless, right token",
    )
    .await;
    assert_ne!(passed.status, 401, "the sessionless handler runs");
    // A cookie that names no session skips the check: `RequireSession` is false there.
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/login/type",
        &login_type,
        &[("Cookie", "MMAUTHTOKEN=mmrsnosuchtokennosuchtok03")],
        "sessionless, dead cookie",
    )
    .await;
    assert_ne!(
        passed.status, 401,
        "a dead cookie on an APIHandler is not a refusal"
    );

    // TrustRequester — both trusted writes answer without the header.
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/roles/names",
        r#"["system_user"]"#,
        &[("Cookie", &cookie)],
        "trusted roles/names",
    )
    .await;
    assert_eq!(passed.status, 200, "roles/names is TrustRequester");
    let passed = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/client_perf",
        "{}",
        &[("Cookie", &cookie)],
        "trusted client_perf",
    )
    .await;
    assert_eq!(passed.status, 200, "client_perf is TrustRequester");

    // The id-less `PUT /posts/ephemeral` stand-in is session-required: no credential is the 401.
    let refused = both(
        &http,
        RUST,
        reqwest::Method::PUT,
        "/api/v4/posts/ephemeral",
        "{}",
        &[],
        "stand-in, anonymous",
    )
    .await;
    assert_eq!(refused.status, 401);
    let refused = both(
        &http,
        RUST,
        reqwest::Method::PUT,
        "/api/v4/posts/ephemeral",
        "{}",
        &[("Cookie", &cookie)],
        "stand-in, cookie",
    )
    .await;
    assert_csrf_refusal(&refused, "the stand-in checks CSRF before its 400");

    // The refusals clear the browser's cookie but do not revoke the session.
    let still = both(
        &http,
        RUST,
        post.clone(),
        "/api/v4/users/ids",
        &ids,
        &[("Authorization", &format!("Bearer {}", session.token))],
        "after",
    )
    .await;
    assert_eq!(still.status, 200, "a CSRF refusal leaves the session alive");

    // ---- strict enforcement ----
    set_go_strict(&http, &admin, true).await;
    let strict = SecondServer::start(
        STRICT_PORT,
        &[(
            "MM_SERVICESETTINGS_EXPERIMENTALSTRICTCSRFENFORCEMENT",
            "true",
        )],
    )
    .await;
    // Everything between the two patches runs under `catch_unwind`: the setting is persisted in
    // the shared configuration, and a panic that skipped the reset would leave every later run
    // strict — failing the lenient legacy-header case above for a reason nowhere near it.
    let outcome = std::panic::AssertUnwindSafe(async {
        match &strict {
            Some(server) => {
                let request = async |headers: &[(&str, &str)], context: &str| {
                    let send = async |base: &str| {
                        let mut request = http
                            .post(format!("{base}/api/v4/users/ids"))
                            .header("Content-Type", "application/json")
                            .body(ids.clone());
                        for (name, value) in headers {
                            request = request.header(*name, *value);
                        }
                        request.send().await.expect("the server answers")
                    };
                    let go = answer(send(GO).await).await;
                    let rs_response = send(&server.base).await;
                    let served = rs_response
                        .headers()
                        .get("x-mmrs-served-by")
                        .is_some_and(|v| v == "rust");
                    (context.to_owned(), go, answer(rs_response).await, served)
                };
                Some((
                    request(
                        &[
                            ("Cookie", &cookie),
                            ("X-CSRF-Token", "notthetoken"),
                            ("X-Requested-With", "XMLHttpRequest"),
                        ],
                        "strict, legacy header",
                    )
                    .await,
                    request(
                        &[("Cookie", &cookie), ("X-CSRF-Token", &session.csrf)],
                        "strict, right token",
                    )
                    .await,
                ))
            }
            None => None,
        }
    })
    .catch_unwind()
    .await;
    drop(strict);
    set_go_strict(&http, &admin, false).await;
    delete_plain_user(&http, &admin, &user.id).await;

    let outcome = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    let (legacy, right) = outcome.expect("the strict mm-api starts");
    let mut compared = Vec::new();
    for (context, go, rs, served) in [legacy, right] {
        assert!(served, "{context}: the strict server forwarded");
        let (go, rs) = comparable(go, rs);
        assert_eq!(go, rs, "{context}");
        compared.push(go);
    }
    assert_csrf_refusal(&compared[0], "strict enforcement refuses the legacy header");
    assert_eq!(
        compared[1].status, 200,
        "strict enforcement still accepts the token"
    );
}

/// `PUT /config/patch` on Go for `ServiceSettings.ExperimentalStrictCSRFEnforcement`.
async fn set_go_strict(http: &reqwest::Client, admin: &str, on: bool) {
    let response = http
        .put(format!("{GO}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "ServiceSettings": { "ExperimentalStrictCSRFEnforcement": on }
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "the strict-CSRF patch is accepted");
}

/// A **valid non-OAuth session token in `?access_token=`** is refused before any handler runs,
/// sessionless or session-required — `api.context.token_provided.app_error`, 401 — and an
/// *unknown* one is not refused at all on a sessionless handler, because a token that resolves to
/// nothing is no session (`RequireSession` is false there).
///
/// Formerly D-810: `CsrfGuard` sits on every sessionless handler for the CSRF half of `ServeHTTP`,
/// and this is the other half of the same block (handlers.go:281).
#[tokio::test]
async fn a_session_token_in_the_query_string_is_refused_before_a_sessionless_handler() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = create_plain_user(&http, &admin, &team, "csrfquery").await;
    let session = browser_login(&http, "csrfquery").await;

    let body = format!(r#"{{"login_id":"{}"}}"#, plain_username("csrfquery"));
    for path in ["/api/v4/users/login/type", "/api/v4/users/login"] {
        let refused = both(
            &http,
            RUST,
            reqwest::Method::POST,
            &format!("{path}?access_token={}", session.token),
            &body,
            &[],
            "a valid session in the query string",
        )
        .await;
        assert_eq!(refused.status, 401, "{path}");
        assert_eq!(
            refused.body["id"], "api.context.token_provided.app_error",
            "{path}"
        );

        let unknown = both(
            &http,
            RUST,
            reqwest::Method::POST,
            &format!("{path}?access_token=mmrsnotasessiontokenatall"),
            &body,
            &[],
            "an unknown token in the query string",
        )
        .await;
        assert_ne!(
            unknown.body["id"], "api.context.token_provided.app_error",
            "{path}: a token that resolves to nothing is not refused"
        );
    }

    // **And before a session-required handler.** The refusal is `ServeHTTP`'s, not the handler's,
    // so `GET /users/me` — `AuthenticatedSession` here — is the same 401, where a port that only
    // guarded the sessionless handlers would authenticate a credential carried in the URL.
    let refused = both(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/users/me?access_token={}", session.token),
        "",
        &[],
        "a valid session in the query string, session required",
    )
    .await;
    assert_eq!(refused.status, 401, "/users/me");
    assert_eq!(
        refused.body["id"], "api.context.token_provided.app_error",
        "/users/me"
    );

    delete_plain_user(&http, &admin, &user.id).await;
}
