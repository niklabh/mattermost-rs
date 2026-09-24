//! Cross-server parity for sliding session expiry — `ExtendSessionExpiryIfNeeded` on its three
//! call sites: `viewChannel`, `createPost` and the websocket's `user_typing`.
//!
//! ```sh
//! scripts/parity.sh --test parity session_expiry
//! ```
//!
//! # The setting is turned on for the suite, and off again
//!
//! `ServiceSettings.ExtendSessionLengthWithActivity` defaults to `!isUpdate`, so it is **off** on
//! the stack's persisted document and both servers do nothing. The suite patches it on through
//! *this* server — a forwarded write that lands in main Go's document, after which
//! `refresh_config_after_write` reloads ours — and patches it off again, at the start as well as
//! the end, so a panic cannot leave a stack with sliding expiry on (which would also disarm
//! `session_activity`'s idle revoke). [`common::SESSION_EXPIRY_SETTING`] is held exclusively for
//! the whole time, and [`common::CONFIG_DOCUMENT`] with it.
//!
//! # The oracle is the row and the `Set-Cookie` headers
//!
//! Every case plants a session with a **fresh token** per server and per run ([`run_nonce`]), so
//! Go's session cache misses
//! and reads the row as planted; the lifetime a case needs is written straight into `CreateAt`
//! and `ExpiresAt`. After the request the suite reads `ExpiresAt` back and collects the response's
//! `Set-Cookie` values, and asserts both servers made the same decision, wrote the same new
//! expiry (to a clock-difference tolerance) and sent the same cookies — with the token value, which
//! differs by construction, and `Expires`, which is a clock read, normalised out and checked
//! separately.
//!
//! The stack's lengths are web 4320 h, mobile 4320 h and SSO 720 h, so the thresholds are one day
//! (web, mobile) and 7.2 hours (SSO). Every "due" case is an hour or more past its threshold and
//! every "not due" one an hour or more short, so clock skew between the processes cannot flip one.

use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};

use crate::common;

use common::{
    CONFIG_DOCUMENT, GO, RUST, SESSION_EXPIRY_SETTING, SocketProbe, client, go_minted_token,
    logged_in_user_id, stack_enabled,
};

const HOUR: i64 = 60 * 60 * 1000;
/// `SessionLengthWebInHours` (and mobile) on the stack's document.
const WEB_LENGTH: i64 = 4320 * HOUR;
/// `SessionLengthSSOInHours` on the stack's document.
const SSO_LENGTH: i64 = 720 * HOUR;
/// The CSRF token every planted session carries, so `MMCSRF` compares equal across servers.
const CSRF: &str = "mmrsextendcsrfmmrsextendcs";

async fn pool() -> sqlx::PgPool {
    common::fixture_pool()
        .await
        .expect("the parity stack exports DATABASE_URL")
}

fn now_millis() -> i64 {
    mm_model::utils::get_millis()
}

/// `PUT {RUST}/api/v4/config/patch` with the one setting — Go's save, then our reload.
async fn set_sliding(http: &reqwest::Client, admin: &str, on: bool) {
    let response = http
        .put(format!("{RUST}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&json!({ "ServiceSettings": { "ExtendSessionLengthWithActivity": on } }))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "patching ExtendSessionLengthWithActivity={on}: {body}"
    );
}

/// The shape of a planted session.
#[derive(Clone, Copy)]
struct Shape {
    /// How long ago the session's current lifetime began.
    elapsed: i64,
    /// The length the lifetime was planted with; `ExpiresAt = now - elapsed + length`.
    planted_length: i64,
    device_id: &'static str,
    is_oauth: bool,
    /// Extra props beyond `csrf`.
    props: &'static [(&'static str, &'static str)],
    /// `ExpiresAt` as planted, overriding the arithmetic above (for `0` and token sessions).
    expires_at: Option<i64>,
}

const WEB: Shape = Shape {
    elapsed: 0,
    planted_length: WEB_LENGTH,
    device_id: "",
    is_oauth: false,
    props: &[],
    expires_at: None,
};

/// Seven base-36 characters of the clock, fixed for the life of this test binary.
///
/// **Not optional.** Go caches sessions by token for `SessionCacheInMinutes` and updates that
/// cache when it extends one, so a token reused by a second run within ten minutes is answered
/// from the *first* run's extended `ExpiresAt`, not from the row planted for the second — the
/// session looks freshly extended and nothing is due. A mutation batch runs this suite every few
/// minutes; with per-tag tokens both no-op controls were reported CAUGHT.
fn run_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(|| {
        let mut n = now_millis() as u64;
        let mut out = Vec::new();
        for _ in 0..7 {
            out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
            n /= 36;
        }
        String::from_utf8(out).expect("ascii")
    })
}

/// Plant a session for the fixture user and return its token.
async fn plant(pool: &sqlx::PgPool, tag: &str, shape: Shape) -> String {
    let nonce = run_nonce();
    let id = format!("mmrsxi{nonce}{tag:0>13}");
    let token = format!("mmrsxt{nonce}{tag:0>13}");
    assert_eq!(id.len(), 26, "{tag}: ids are 26 characters");
    assert_eq!(token.len(), 26, "{tag}: tokens are 26 characters");

    let now = now_millis();
    let expires_at = shape
        .expires_at
        .unwrap_or(now - shape.elapsed + shape.planted_length);
    let mut props = serde_json::Map::new();
    props.insert("csrf".to_owned(), Value::from(CSRF));
    for (key, value) in shape.props {
        props.insert((*key).to_owned(), Value::from(*value));
    }

    sqlx::query("DELETE FROM sessions WHERE id = $1 OR token = $2")
        .bind(&id)
        .bind(&token)
        .execute(pool)
        .await
        .expect("clears a leftover");
    sqlx::query(
        "INSERT INTO sessions
             (id, token, createat, expiresat, lastactivityat, userid, deviceid, roles,
              isoauth, props, expirednotify, voipdeviceid)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'system_user system_admin', $8, $9, true, '')",
    )
    .bind(&id)
    .bind(&token)
    .bind(now - shape.elapsed)
    .bind(expires_at)
    .bind(now)
    .bind(logged_in_user_id())
    .bind(shape.device_id)
    .bind(shape.is_oauth)
    .bind(Value::Object(props))
    .execute(pool)
    .await
    .expect("plants the session");
    token
}

async fn row(pool: &sqlx::PgPool, token: &str) -> (i64, bool) {
    sqlx::query_as("SELECT expiresat, expirednotify FROM sessions WHERE token = $1")
        .bind(token)
        .fetch_one(pool)
        .await
        .expect("the planted session is still there")
}

async fn unplant(pool: &sqlx::PgPool, tokens: &[String]) {
    for token in tokens {
        sqlx::query("DELETE FROM sessions WHERE token = $1")
            .bind(token)
            .execute(pool)
            .await
            .expect("removes the planted session");
    }
}

/// Which call site a case goes through.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    /// `POST /channels/members/me/view` with blank ids — the focus-loss view, which writes no
    /// member row.
    View,
    /// `POST /posts` into the fixture channel.
    Post,
    /// `user_typing` over the websocket, with a bad `channel_id`: refused, but the extension runs
    /// first.
    Typing,
}

/// What one server did with one case.
#[derive(Debug)]
struct Outcome {
    status: u16,
    before: i64,
    after: i64,
    expired_notify: bool,
    /// When the request was sent, for the tolerance checks.
    sent_at: i64,
    set_cookies: Vec<String>,
}

async fn send(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    via: Via,
    channel_id: &str,
    extra: &[(&str, &str)],
) -> (u16, Vec<String>, Option<String>) {
    let request = match via {
        Via::View => http
            .post(format!("{base}/api/v4/channels/members/me/view"))
            .body(r#"{"channel_id":"","prev_channel_id":""}"#),
        Via::Post => http
            .post(format!("{base}/api/v4/posts"))
            .body(json!({"channel_id": channel_id, "message": "mmrs session expiry"}).to_string()),
        Via::Typing => {
            let mut socket = SocketProbe::connect(base, token).await;
            socket
                .send(json!({"seq": 1, "action": "user_typing", "data": {"channel_id": "short"}}))
                .await;
            let answered = socket
                .collect_until(Duration::from_secs(5), |frames| {
                    frames.iter().any(|f| f["seq_reply"] == 1)
                })
                .await;
            assert!(answered, "{base}: no answer to user_typing");
            let answer = socket
                .frames()
                .into_iter()
                .find(|f| f["seq_reply"] == 1)
                .expect("found above");
            assert_eq!(
                answer["error"]["id"], "api.websocket_handler.invalid_param.app_error",
                "{base}: {answer}"
            );
            socket.close().await;
            return (400, Vec::new(), None);
        }
    };
    let mut request = request
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().expect("an ASCII cookie").to_owned())
        .collect();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if via == Via::Post && status == 201 {
        let post: Value = response.json().await.expect("a post");
        common::delete_post(http, token, post["id"].as_str().expect("an id")).await;
    }
    (status, cookies, served_by)
}

/// Run one case on both servers and return `(go, rust)`. Everything a case needs besides its
/// own shape, and every token it planted, for the cleanup.
struct Cases<'a> {
    http: &'a reqwest::Client,
    pool: &'a sqlx::PgPool,
    channel_id: &'a str,
    planted: Vec<String>,
}

impl Cases<'_> {
    async fn run(
        &mut self,
        tag: &str,
        shape: Shape,
        via: Via,
        extra: &[(&str, &str)],
    ) -> (Outcome, Outcome) {
        let (http, pool, channel_id) = (self.http, self.pool, self.channel_id);
        let mut outcomes = Vec::new();
        for (base, side) in [(GO, "g"), (RUST, "r")] {
            let token = plant(pool, &format!("{tag}{side}"), shape).await;
            self.planted.push(token.clone());
            let (before, _) = row(pool, &token).await;
            let sent_at = now_millis();
            let (status, set_cookies, served_by) =
                send(http, base, &token, via, channel_id, extra).await;
            if base == RUST && !matches!(via, Via::Typing) {
                assert_eq!(
                    served_by.as_deref(),
                    Some("rust"),
                    "{tag}: {via:?} was forwarded, so this proves nothing about the Rust handler"
                );
            }
            let (after, expired_notify) = row(pool, &token).await;
            // The token is a per-server value by construction; name it instead.
            let set_cookies = set_cookies
                .into_iter()
                .map(|cookie| cookie.replace(&token, "<token>"))
                .collect();
            outcomes.push(Outcome {
                status,
                before,
                after,
                expired_notify,
                sent_at,
                set_cookies,
            });
        }
        let rust = outcomes.pop().expect("two outcomes");
        let go = outcomes.pop().expect("two outcomes");
        (go, rust)
    }
}

/// `Expires=` out of a cookie, as Unix seconds, and the cookie without it.
fn split_expires(cookie: &str) -> (Option<i64>, String) {
    let mut expires = None;
    let kept: Vec<&str> = cookie
        .split("; ")
        .filter(|attribute| match attribute.strip_prefix("Expires=") {
            Some(value) => {
                expires = chrono::DateTime::parse_from_rfc2822(&value.replace("GMT", "+0000"))
                    .ok()
                    .map(|t| t.timestamp());
                false
            }
            None => true,
        })
        .collect();
    (expires, kept.join("; "))
}

/// Both servers extended the session to `now + length` and sent the same three cookies.
fn assert_extended(tag: &str, go: &Outcome, rust: &Outcome, length: i64, cookies: bool) {
    for (who, outcome) in [("go", go), ("rust", rust)] {
        assert_ne!(outcome.after, outcome.before, "{tag}: {who} did not extend");
        let expected = outcome.sent_at + length;
        assert!(
            (outcome.after - expected).abs() < 30_000,
            "{tag}: {who} wrote {} where now + length is {expected}",
            outcome.after
        );
        assert!(
            !outcome.expired_notify,
            "{tag}: {who} left ExpiredNotify set; UpdateExpiresAt clears it"
        );
    }
    assert_eq!(go.status, rust.status, "{tag}: status");
    if !cookies {
        assert!(go.set_cookies.is_empty(), "{tag}: go {:?}", go.set_cookies);
        assert!(
            rust.set_cookies.is_empty(),
            "{tag}: rust {:?}",
            rust.set_cookies
        );
        return;
    }
    assert_eq!(
        go.set_cookies.len(),
        3,
        "{tag}: go sent {:?}",
        go.set_cookies
    );
    let normalise = |outcome: &Outcome| -> Vec<String> {
        outcome
            .set_cookies
            .iter()
            .map(|cookie| {
                let (expires, rest) = split_expires(cookie);
                let expires = expires.unwrap_or_else(|| panic!("{tag}: no Expires in {cookie}"));
                // `GetMillis()/1000 + maxAgeSeconds`, the web length whatever the session.
                let expected = outcome.sent_at / 1000 + WEB_LENGTH / 1000;
                assert!(
                    (expires - expected).abs() < 30,
                    "{tag}: Expires {expires} is not now + the web length ({expected}) in {cookie}"
                );
                rest
            })
            .collect()
    };
    assert_eq!(normalise(go), normalise(rust), "{tag}: Set-Cookie");
}

/// Neither server touched the row or sent a cookie.
fn assert_untouched(tag: &str, go: &Outcome, rust: &Outcome) {
    for (who, outcome) in [("go", go), ("rust", rust)] {
        assert_eq!(outcome.after, outcome.before, "{tag}: {who} extended");
        assert!(
            outcome.expired_notify,
            "{tag}: {who} rewrote the row; ExpiredNotify was cleared"
        );
        assert!(
            outcome.set_cookies.is_empty(),
            "{tag}: {who} sent {:?}",
            outcome.set_cookies
        );
    }
    assert_eq!(go.status, rust.status, "{tag}: status");
}

/// Every case with the setting **on**.
#[tokio::test]
async fn sliding_expiry_matches_go_on_every_call_site() {
    if !stack_enabled() {
        return;
    }
    let _document = CONFIG_DOCUMENT.write().await;
    let _setting = SESSION_EXPIRY_SETTING.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    // A channel of its own: the shared fixture channel is written by every post suite, and a
    // create into it was measured forwarded under the full run's concurrency.
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel_id = common::create_channel(&http, &admin, &team_id, "sessexp").await;
    let pool = pool().await;
    let mut cases = Cases {
        http: &http,
        pool: &pool,
        channel_id: &channel_id,
        planted: Vec::new(),
    };

    set_sliding(&http, &admin, false).await;
    set_sliding(&http, &admin, true).await;
    // `AssertUnwindSafe`: on a panic nothing inside is reused but the list of planted tokens,
    // which only ever grows and is complete up to the panic.
    let outcome = std::panic::AssertUnwindSafe(async {
        const DAY: i64 = 24 * HOUR;

        // A web session a day and an hour into its lifetime: due, on each call site.
        for (tag, via) in [("webview", Via::View), ("webpost", Via::Post)] {
            let shape = Shape {
                elapsed: DAY + HOUR,
                ..WEB
            };
            let (go, rust) = cases.run(tag, shape, via, &[]).await;
            assert_eq!(go.status, if via == Via::View { 200 } else { 201 }, "{tag}");
            assert_extended(tag, &go, &rust, WEB_LENGTH, true);
        }

        // An hour short of the day: not due.
        for (tag, via) in [("webshortview", Via::View), ("webshortpost", Via::Post)] {
            let shape = Shape {
                elapsed: DAY - HOUR,
                ..WEB
            };
            let (go, rust) = cases.run(tag, shape, via, &[]).await;
            assert_untouched(tag, &go, &rust);
        }

        // The websocket: the app method, so the row moves and there is no response to carry a
        // cookie — and it runs before the refusal.
        let shape = Shape {
            elapsed: DAY + HOUR,
            ..WEB
        };
        let (go, rust) = cases.run("typing", shape, Via::Typing, &[]).await;
        assert_extended("typing", &go, &rust, WEB_LENGTH, false);
        let shape = Shape {
            elapsed: DAY - HOUR,
            ..WEB
        };
        let (go, rust) = cases.run("typingshort", shape, Via::Typing, &[]).await;
        assert_untouched("typingshort", &go, &rust);

        // SSO (an OAuth login's prop): its own 7.2-hour threshold and its own length. Ten hours
        // is due here and would not be for a web session. The cookies stay on the web length.
        let sso = Shape {
            elapsed: 10 * HOUR,
            planted_length: SSO_LENGTH,
            props: &[("isOAuthUser", "true")],
            ..WEB
        };
        let (go, rust) = cases.run("sso", sso, Via::View, &[]).await;
        assert_extended("sso", &go, &rust, SSO_LENGTH, true);

        // Mobile is tested before SSO: the same prop plus a device id takes the mobile length,
        // whose day threshold ten hours does not reach.
        let phone = Shape {
            elapsed: 10 * HOUR,
            device_id: "apple_rn:mmrsextenddevice",
            props: &[("isOAuthUser", "true")],
            ..WEB
        };
        let (go, rust) = cases.run("phone", phone, Via::View, &[]).await;
        assert_untouched("phone", &go, &rust);

        // …and past the day it is extended by the mobile length.
        let phone_due = Shape {
            elapsed: DAY + HOUR,
            ..phone
        };
        let (go, rust) = cases.run("phonedue", phone_due, Via::View, &[]).await;
        assert_extended("phonedue", &go, &rust, WEB_LENGTH, true);

        // An OAuth-app session (the `IsOAuth` column) is not exempt and not SSO.
        let oauth_app = Shape {
            elapsed: DAY + HOUR,
            is_oauth: true,
            ..WEB
        };
        let (go, rust) = cases.run("oauthapp", oauth_app, Via::View, &[]).await;
        assert_extended("oauthapp", &go, &rust, WEB_LENGTH, true);

        // A personal access token's session with an expiry: never due, however old. A **bot's**,
        // because `EnableUserAccessTokens` is off on the stack and `SessionRequired` refuses a
        // human's token session before any handler runs (see the last test below).
        let pat = Shape {
            elapsed: 300 * DAY,
            props: &[
                ("type", "UserAccessToken"),
                ("user_access_token_id", "x"),
                ("is_bot", "true"),
            ],
            expires_at: Some(now_millis() + 10 * DAY),
            ..WEB
        };
        let (go, rust) = cases.run("pat", pat, Via::View, &[]).await;
        assert_eq!(go.status, 200, "pat: a bot's token session is served");
        assert_untouched("pat", &go, &rust);

        // …and one with **no** expiry is an ordinary web session: due, and given one.
        let pat_forever = Shape {
            elapsed: HOUR,
            expires_at: Some(0),
            ..pat
        };
        let (go, rust) = cases.run("patforever", pat_forever, Via::View, &[]).await;
        assert_extended("patforever", &go, &rust, WEB_LENGTH, true);

        // A never-expiring session (`ExpiresAt = 0`) is always due, and comes out expiring.
        let forever = Shape {
            elapsed: HOUR,
            expires_at: Some(0),
            ..WEB
        };
        let (go, rust) = cases.run("forever", forever, Via::View, &[]).await;
        assert_extended("forever", &go, &rust, WEB_LENGTH, true);

        // Behind a TLS proxy and embedded: `Secure` and `SameSite=None` on all three.
        let shape = Shape {
            elapsed: DAY + HOUR,
            ..WEB
        };
        let (go, rust) = cases
            .run(
                "embedded",
                shape,
                Via::View,
                &[("X-Forwarded-Proto", "https"), ("Cookie", "MMEMBED=1")],
            )
            .await;
        assert_extended("embedded", &go, &rust, WEB_LENGTH, true);
        for cookie in &rust.set_cookies {
            assert!(cookie.contains("; Secure; SameSite=None"), "{cookie}");
        }
    })
    .catch_unwind()
    .await;

    set_sliding(&http, &admin, false).await;
    unplant(&pool, &cases.planted).await;
    common::delete_channel(&http, &admin, &channel_id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// With the setting **off** — the stack's own state — a session well past the threshold is left
/// alone by both servers, on every call site.
#[tokio::test]
async fn nothing_slides_with_the_setting_off() {
    if !stack_enabled() {
        return;
    }
    let _setting = SESSION_EXPIRY_SETTING.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    // A channel of its own: the shared fixture channel is written by every post suite, and a
    // create into it was measured forwarded under the full run's concurrency.
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel_id = common::create_channel(&http, &admin, &team_id, "sessexpoff").await;
    let pool = pool().await;
    let mut cases = Cases {
        http: &http,
        pool: &pool,
        channel_id: &channel_id,
        planted: Vec::new(),
    };

    let shape = Shape {
        elapsed: 30 * 24 * HOUR,
        ..WEB
    };
    for (tag, via) in [
        ("offview", Via::View),
        ("offpost", Via::Post),
        ("offtyping", Via::Typing),
    ] {
        let (go, rust) = cases.run(tag, shape, via, &[]).await;
        assert_untouched(tag, &go, &rust);
    }
    unplant(&pool, &cases.planted).await;
    common::delete_channel(&http, &admin, &channel_id).await;
}

/// **`SessionRequired`**, found by this suite: with `EnableUserAccessTokens` off — the stack's
/// default — a session minted from a *human's* personal access token is refused before the handler
/// runs, the generic 401 with no cookie cleared. This server served it until the port in
/// `crate::auth::session_required`; a planted token session for the sliding-expiry cases got a 200
/// here and a 401 from Go.
#[tokio::test]
async fn a_humans_token_session_is_refused_while_tokens_are_off() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    // Only so the fixture user's id is known.
    let _admin = go_minted_token(&http).await;
    let pool = pool().await;

    let human_pat = Shape {
        props: &[("type", "UserAccessToken"), ("user_access_token_id", "x")],
        expires_at: Some(now_millis() + 24 * HOUR),
        ..WEB
    };
    let mut planted = Vec::new();
    let mut bodies = Vec::new();
    for (base, tag) in [(GO, "humanpatg"), (RUST, "humanpatr")] {
        let token = plant(&pool, tag, human_pat).await;
        planted.push(token.clone());
        let response = http
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status().as_u16(), 401, "{base}");
        assert!(
            response.headers().get("set-cookie").is_none(),
            "{base}: SessionRequired clears no cookie"
        );
        bodies.push(response.bytes().await.expect("a body").to_vec());
    }
    let go = common::assert_error_bodies_match_except_known_gaps(
        &bodies[0],
        &bodies[1],
        "a human's token session with tokens off",
    );
    assert_eq!(go["id"], "api.context.session_expired.app_error");
    unplant(&pool, &planted).await;
}
