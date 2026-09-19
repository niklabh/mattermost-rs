//! `GET /manualtest` — port of `manualtesting.ManualTest` (channels/manualtesting/
//! manual_testing.go:37, test_autolink.go), which `api4.Init` registers only when
//! `ServiceSettings.EnableTesting` is on at start (api4/api.go:414). Off, the path is the web
//! client's page (`web_static::root`).
//!
//! It is an `APIHandler` on the **root** router, not under `/api/`, so every error it raises is
//! `RenderWebAppError`'s signed page ([`crate::web_error`]) — JSON only for a client sending
//! `X-Mobile-App`.
//!
//! # What the handler reads as it is written, and what it does at the pinned SHA
//!
//! The source reads as a fixture builder: seed `math/rand` from `uid`, create a team, its
//! `town-square`, a user through the server's own REST API, verify and join them, log in, set the
//! session cookie and redirect, then run the named test. Three facts about the pinned tree make
//! almost none of that reachable, and each is a property of constants in the handler, not of
//! data:
//!
//! 1. **`rand.Seed` is a no-op.** The server is built with Go 1.26 and its `go.mod` says
//!    `go 1.26.4`, so `GODEBUG=randseednop=1` is the default: since Go 1.24 the top-level
//!    `Seed` returns immediately (math/rand/rand.go:400). The FNV-1a hash of `uid` + the clock is
//!    computed and discarded. Nothing here computes it.
//! 2. **The team can never be saved.** Its email is `"success+" + NewId() + "simulator.amazonses
//!    .com"` — no `@` — and `Team.IsValid` checks the email before anything that depends on the
//!    request (only the id and timestamps `PreSave` just set come first). So `username` +
//!    `teamname` is always the 400 `model.team.is_valid.email.app_error`, before any write and
//!    whatever the two values are. The user it would have created has the same kind of email and
//!    would fail `User.IsValid` the same way; the login, the join, the cookie and the redirect
//!    are behind both.
//! 3. **`test=autolink` without a created user looks for a channel as user `""`.**
//!    `getChannelID(App, "town-square", "", "")` is `GetChannels` with no team filter and
//!    `cm.UserId = ''`, which finds nothing unless a `ChannelMembers` row with an empty user id
//!    exists — so it is the 500 `manaultesting.test_autolink.unable.app_error`. When such a row
//!    does exist, the post goes through `model.Client4` with **no token** (the login never ran),
//!    which the server's own `createPost` refuses with the 401
//!    `api.context.session_expired.app_error` (`SessionRequired`, detail `UserRequired`), and
//!    that `AppError` becomes this request's. Rust answers that 401 itself rather than calling
//!    itself: the refusal happens in `ServeHTTP` before `createPost` reads anything.
//!
//! What remains: the query parse and its 400, the team build and save with Go's error mapping,
//! the `test` dispatch, and an empty 200 for any other `test` value.
//!
//! # What is forwarded
//!
//! - A team save that **succeeds**. It cannot (2.), and if it did, the handler's next step is
//!   Go's own REST client against Go's own listen address. A saved team would be orphaned, and Go
//!   answers.
//! - The `autolink` post when `ListenAddress` does not start with `:`. `"http://localhost" +
//!   ListenAddress` is only a URL for Go's own port in that form (`localhost127.0.0.1:8065` is a
//!   host name); what `Client4` returns for a connection that fails is not something this port
//!   reads.
//! - Whatever [`crate::web_error::handle_context_error`] and
//!   [`crate::web_static::session_preamble`] forward.

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use mm_model::go_url::parse_query;
use mm_model::utils::AppError;

use crate::AppState;
use crate::serve_content::{build_response, set_header};
use crate::web_error::{ErrorContext, handle_context_error};
use crate::web_static::{Preamble, StaticSetup, serve_http_headers, session_preamble};

/// `manaultesting.manual_test.parse.app_error` — Go's spelling, typo included.
const PARSE_ERROR: &str = "manaultesting.manual_test.parse.app_error";
/// `manaultesting.test_autolink.unable.app_error`.
const AUTOLINK_UNABLE: &str = "manaultesting.test_autolink.unable.app_error";

/// Every error id this handler can put on the page, for the translation guard in the tests.
#[cfg(test)]
const RAISED_IDS: [&str; 8] = [
    PARSE_ERROR,
    AUTOLINK_UNABLE,
    "model.team.is_valid.email.app_error",
    "app.team.save.existing.app_error",
    "app.team.save.app_error",
    "api.context.session_expired.app_error",
    "api.context.token_provided.app_error",
    "basic_security_check.url.too_long_error",
];

/// What the handler body decided.
enum Outcome {
    /// Nothing written: `net/http`'s empty 200.
    Ok,
    Err(AppError),
    Forward,
}

/// `web.Handler.ServeHTTP` around `ManualTest`: `basicSecurityChecks`, the headers, the session
/// half, the handler, `handleContextError`, and the `gzhttp` wrapper `APIHandler` adds in `gzip`
/// mode. `None` forwards.
#[tracing::instrument(skip_all)]
pub(crate) async fn manual_test(
    state: &AppState,
    setup: &StaticSetup,
    raw_target: &str,
    raw_query: &str,
    parts: &Parts,
) -> Option<Response> {
    let config = mm_app::config::load_model_config(state.app.store().config())
        .await
        .map_err(|err| tracing::warn!(error = %err, "could not read the configuration"))
        .ok()?;
    let service = &config.service_settings;
    let request_id = mm_model::utils::new_id();
    let accept_language = header_str(parts, header::ACCEPT_LANGUAGE.as_str());
    let context = ErrorContext {
        accept_language,
        mobile_app: !header_str(parts, "x-mobile-app").is_empty(),
        request_id: &request_id,
    };

    // `basicSecurityChecks` runs before any header is set, so its page carries none of them.
    let max_url = service.maximum_url_length.unwrap_or(2048);
    let answer = if i64::try_from(raw_target.len()).unwrap_or(i64::MAX) > max_url {
        let err = AppError::new(
            "basicSecurityChecks",
            "basic_security_check.url.too_long_error",
            None,
            "",
            414,
        );
        handle_context_error(state, &context, HeaderMap::new(), err).await?
    } else {
        let mut headers = serve_http_headers(state, &config, &request_id).await?;
        // `IsStatic` is false: every API response is JSON by default, and a `GET` never cached.
        set_header(&mut headers, "content-type", "application/json");
        set_header(&mut headers, "expires", "0");

        let outcome = match session_preamble(state, parts).await {
            Preamble::Forward => Outcome::Forward,
            Preamble::Error(err) => Outcome::Err(*err),
            Preamble::Continue => {
                let listen_address = service.listen_address.as_deref().unwrap_or(":8065");
                handle(state, raw_query, listen_address).await
            }
        };
        match outcome {
            Outcome::Forward => return None,
            Outcome::Ok => build_response(StatusCode::OK, headers, Body::empty()),
            Outcome::Err(err) => handle_context_error(state, &context, headers, err).await?,
        }
    };

    if setup_is_gzip(setup) {
        let accept_encoding = parts
            .headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        return crate::gzhttp::wrap(&parts.method, accept_encoding, answer)
            .await
            .map_err(|err| tracing::error!(error = %err, "compressing /manualtest failed"))
            .ok();
    }
    Some(answer)
}

fn setup_is_gzip(setup: &StaticSetup) -> bool {
    setup.webserver_mode() == "gzip"
}

/// `r.Header.Get(name)`, as text; a value that is not UTF-8 matches nothing Go compares it with,
/// which is what the empty string does too.
fn header_str<'a>(parts: &'a Parts, name: &str) -> &'a str {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// `ManualTest` itself (manual_testing.go:37).
async fn handle(state: &AppState, raw_query: &str, listen_address: &str) -> Outcome {
    let (params, parse_error) = parse_query(raw_query);
    if parse_error.is_some() {
        return Outcome::Err(AppError::new("/manual", PARSE_ERROR, None, "", 400));
    }

    // `uid` seeds `math/rand` through `rand.Seed`, a no-op at the pinned toolchain — see the
    // module doc.

    if let (Some(_), Some(team_display_name)) =
        (params.get_all("username"), params.get_all("teamname"))
    {
        let display_name = team_display_name
            .first()
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .unwrap_or_default();
        let mut team = mm_model::team::Team {
            display_name,
            name: format!(
                "zz{}",
                mm_model::utils::rand_string(20, mm_model::utils::LOWERCASE)
            ),
            email: format!(
                "success+{}simulator.amazonses.com",
                mm_model::utils::new_id()
            ),
            team_type: mm_model::team::TEAM_OPEN.to_owned(),
            ..Default::default()
        };
        return match state.app.save_team(&mut team).await {
            Err(mm_store::StoreError::InvalidInput { .. }) => Outcome::Err(AppError::new(
                "manualTest",
                "app.team.save.existing.app_error",
                None,
                "",
                400,
            )),
            Err(mm_store::StoreError::Invalid { app_error, .. }) => Outcome::Err(*app_error),
            Err(other) => Outcome::Err(
                AppError::new("manualTest", "app.team.save.app_error", None, "", 500).wrap(other),
            ),
            Ok(saved) => {
                tracing::error!(team_id = %saved.id, "a /manualtest team with no @ in its email was saved");
                Outcome::Forward
            }
        };
    }

    let Some(test_name) = params.get_all("test") else {
        return Outcome::Err(AppError::new("/manual", PARSE_ERROR, None, "", 400));
    };
    match test_name.first().map(Vec::as_slice) {
        Some(b"autolink") => test_auto_link(state, listen_address).await,
        _ => Outcome::Ok,
    }
}

/// `testAutoLink` (test_autolink.go:28) with the empty team and user ids `ManualTest` passes
/// when it created neither — the only way to reach it.
async fn test_auto_link(state: &AppState, listen_address: &str) -> Outcome {
    if state
        .app
        .manual_test_channel_id(mm_model::channel::DEFAULT_CHANNEL_NAME, "", "")
        .await
        .is_none()
    {
        return Outcome::Err(AppError::new("/manualtest", AUTOLINK_UNABLE, None, "", 500));
    }
    // `env.Client.CreatePost` with no token: `SessionRequired` on Go's own `createPost`.
    if !listen_address.starts_with(':') {
        return Outcome::Forward;
    }
    Outcome::Err(AppError::new(
        "",
        "api.context.session_expired.app_error",
        None,
        "UserRequired",
        401,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id this handler can raise has a plain translation in every supported locale's file,
    /// or none: [`mm_app::i18n::Translations`] keeps a `{{`-template raw where go-i18n would
    /// render it, so an id that grew one would put different text under the signature.
    #[test]
    fn no_raised_id_has_a_template_translation() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        if !dir.is_dir() {
            return;
        }
        for locale in mm_app::i18n::SUPPORTED_LOCALES {
            let text = std::fs::read_to_string(dir.join(format!("{locale}.json"))).unwrap();
            let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
            for entry in entries {
                let id = entry["id"].as_str().unwrap_or_default();
                if RAISED_IDS.contains(&id) {
                    let translation = &entry["translation"];
                    assert!(
                        translation.as_str().is_some_and(|t| !t.contains("{{")),
                        "{locale}: {id} is {translation}"
                    );
                }
            }
        }
    }

    /// The premise of the module doc's second point, against the model's own validator: the team
    /// this handler builds fails `IsValid` on its email whatever the request says.
    #[test]
    fn the_handlers_team_never_validates() {
        for display_name in ["", "x", &"y".repeat(100)] {
            let mut team = mm_model::team::Team {
                display_name: display_name.to_owned(),
                name: format!(
                    "zz{}",
                    mm_model::utils::rand_string(20, mm_model::utils::LOWERCASE)
                ),
                email: format!(
                    "success+{}simulator.amazonses.com",
                    mm_model::utils::new_id()
                ),
                team_type: mm_model::team::TEAM_OPEN.to_owned(),
                ..Default::default()
            };
            team.pre_save();
            let err = team.is_valid().expect_err("never valid");
            assert_eq!(err.id, "model.team.is_valid.email.app_error");
            assert_eq!(err.status_code, 400);
        }
    }
}
