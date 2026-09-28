//! Port of `Context.ExtendSessionExpiryIfNeeded` (web/context.go:174) and of the cookie half it
//! calls, `App.AttachSessionCookies` (app/login.go:288) with its cloud tail
//! `App.AttachCloudSessionCookie` (app/login.go:243).
//!
//! Go calls the context method from two REST handlers — `createPost` (api4/post.go:184) and
//! `viewChannel` (api4/channel.go:2053), both after `UpdateLastActivityAtIfNeeded` and before the
//! status line — and the app method, **without** cookies, from the websocket's `user_typing`
//! (wsapi/user.go:17). Those are all of its callers in the tree, and all three are served here.
//!
//! The decision and the row write are `mm_app`'s ([`mm_app::App::extend_session_expiry_if_needed`]);
//! what lives here is the wire-visible part: three (or four) `Set-Cookie` headers carrying the
//! session's token, user id and CSRF token again with a fresh `Max-Age`, so a browser's cookies
//! slide along with the row.

use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use mm_app::config::Config;
use mm_model::session::{SESSION_COOKIE_CLOUD_URL, Session};
use mm_model::utils::get_millis;

use crate::AppState;
use crate::login::session_cookies;
use crate::sessions::render_session_cookie;

/// Port of `Context.ExtendSessionExpiryIfNeeded` (web/context.go:174): extend the session, and
/// when that happened return the session cookies for the response.
///
/// Called where Go calls it — after the handler's work and **before** its response is built — so
/// the row is written even when building the body then fails, as in Go, where the call precedes
/// `WriteHeader`. The caller applies the result with [`SessionCookies::apply`].
pub(crate) async fn extend_session_expiry_if_needed(
    state: &AppState,
    request_headers: &HeaderMap,
    session: &Session,
) -> SessionCookies {
    if !state.app.extend_session_expiry_if_needed(session).await {
        return SessionCookies(Vec::new());
    }
    let values = attach_session_cookies(state, request_headers, session)
        .await
        .into_iter()
        .filter_map(|cookie| match HeaderValue::from_str(&cookie) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::error!(error = %err, "a session cookie is not a header value");
                None
            }
        })
        .collect();
    SessionCookies(values)
}

/// The `Set-Cookie` values [`extend_session_expiry_if_needed`] owes the response — none when the
/// session was not extended.
#[must_use = "the cookies have to reach the response"]
pub(crate) struct SessionCookies(Vec<HeaderValue>);

impl SessionCookies {
    /// Append the cookies to `response`, in order. Appended, never replacing a `Set-Cookie` the
    /// handler set itself; neither served caller sets one.
    pub(crate) fn apply(self, mut response: Response) -> Response {
        for value in self.0 {
            response.headers_mut().append("Set-Cookie", value);
        }
        response
    }
}

/// Port of `App.AttachSessionCookies` (app/login.go:288), in emission order: `MMAUTHTOKEN`,
/// `MMUSERID`, `MMCSRF` ([`session_cookies`], shared with `login`), then `MMCLOUDURL` when the
/// licence is a cloud one.
///
/// Go's `License()` is an in-memory read that cannot fail. Ours reads the store; a failure is
/// logged and treated as "not cloud", so the three cookies still go out — the request has
/// already succeeded and the session row is already extended, and failing it now would tell the
/// client the opposite.
pub(crate) async fn attach_session_cookies(
    state: &AppState,
    request_headers: &HeaderMap,
    session: &Session,
) -> Vec<String> {
    let mut cookies = session_cookies(state, request_headers, session).to_vec();

    let cloud = match state.app.license().await {
        Ok(license) => license.is_some_and(|license| license.is_cloud()),
        Err(err) => {
            tracing::error!(error = %err, "could not read the licence for the cloud cookie");
            false
        }
    };
    if cloud
        && let Some(cookie) = cloud_session_cookie(
            &state.app.config(),
            is_https(request_headers),
            get_millis() / 1000,
        )
    {
        cookies.push(cookie);
    }
    cookies
}

/// `GetProtocol(r) == "https"`: `X-Forwarded-Proto: https`, exactly. The TLS arm cannot fire —
/// this server terminates no TLS.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("X-Forwarded-Proto")
        .and_then(|value| value.to_str().ok())
        == Some("https")
}

/// Port of `App.AttachCloudSessionCookie` (app/login.go:243), with the protocol and the clock
/// (whole seconds) as parameters.
///
/// The cookie names the **workspace**, taken from `SiteURL`'s hostname:
///
/// - no hostname (an empty or unparseable SiteURL) → no cookie;
/// - a hostname that **contains** `localhost` anywhere → workspace `localhost`, and the domain is
///   the whole hostname, so `mylocalhost.example.com` takes this arm too;
/// - otherwise the hostname must be **exactly four** dot-separated labels, or no cookie; the
///   workspace is the first and the domain is `.` plus the third and fourth — one level *above*
///   `cloud.mattermost.com`, so the cookie is visible to every workspace. `net/http` then drops the
///   leading dot, and drops the whole `Domain` of a four-label IPv4 literal (no letter).
///
/// Unlike the three session cookies it has no `HttpOnly` and never `SameSite`.
fn cloud_session_cookie(config: &Config, secure: bool, now_seconds: i64) -> Option<String> {
    let max_age_seconds = config.session_length_web_in_hours * 60 * 60;
    let subpath = config.subpath();
    let expires_at = now_seconds + max_age_seconds;

    let hostname = mm_model::go_url::go_parse(config.site_url.as_deref().unwrap_or(""))
        .map(|url| String::from_utf8_lossy(&url.hostname()).into_owned())
        .unwrap_or_default();
    if hostname.is_empty() {
        return None;
    }

    let (workspace, domain) = if hostname.contains("localhost") {
        ("localhost".to_owned(), hostname)
    } else {
        let labels: Vec<&str> = hostname.split('.').collect();
        let [workspace, _, second_level, top_level] = labels.as_slice() else {
            return None;
        };
        (
            (*workspace).to_owned(),
            format!(".{second_level}.{top_level}"),
        )
    };

    Some(render_session_cookie(
        SESSION_COOKIE_CLOUD_URL,
        &workspace,
        &subpath,
        &domain,
        max_age_seconds,
        expires_at,
        false,
        secure,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/behaviour_session_write.json"
        ))
        .expect("the oracle is generated by reference/dump");
        serde_json::from_str(&raw).expect("the oracle is JSON")
    }

    /// Every SiteURL in `cloud_cookie`, both protocols, against `AttachCloudSessionCookie`'s glue
    /// over Go's own `url.Parse`, `strings` and `Cookie.String` — including every exit that sets
    /// no cookie at all.
    #[test]
    fn the_cloud_cookie_matches_gos_bytes() {
        let oracle = oracle();
        let cases = oracle["cloud_cookie"]
            .as_array()
            .expect("the section is an array");
        assert!(cases.len() >= 40, "the corpus shrank");
        assert!(cases.iter().any(|c| c["set_cookie"] == ""));
        assert!(cases.iter().any(|c| c["set_cookie"] != ""));

        for case in cases {
            let site_url = case["site_url"].as_str().expect("a site url");
            let secure = case["secure"].as_bool().expect("a secure flag");
            let max_age = case["max_age"].as_i64().expect("a max age");
            let expires = case["expires_unix"].as_i64().expect("an expiry");
            let config = Config {
                site_url: Some(site_url.to_owned()),
                session_length_web_in_hours: max_age / 3600,
                ..Config::default()
            };
            let rendered = cloud_session_cookie(&config, secure, expires - max_age);
            assert_eq!(
                rendered.as_deref().unwrap_or(""),
                case["set_cookie"].as_str().expect("a rendered cookie"),
                "cloud cookie for {site_url:?}, secure={secure}"
            );
        }
    }

    /// An absent SiteURL is Go's `""`, which has no hostname: no cookie.
    #[test]
    fn no_site_url_is_no_cloud_cookie() {
        let config = Config {
            site_url: None,
            ..Config::default()
        };
        assert_eq!(cloud_session_cookie(&config, false, 0), None);
    }

    /// The protocol test is an exact match on `X-Forwarded-Proto`.
    #[test]
    fn https_is_the_forwarded_proto_exactly() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                "X-Forwarded-Proto",
                HeaderValue::from_str(value).expect("ascii"),
            );
            is_https(&headers)
        };
        assert!(with("https"));
        assert!(!with("HTTPS"));
        assert!(!with("http"));
        assert!(!is_https(&HeaderMap::new()));
    }
}
