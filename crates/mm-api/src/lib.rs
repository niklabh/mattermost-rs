//! REST API and the Strangler Fig proxy.
//!
//! The router serves the routes that have been migrated and forwards everything else to the still
//! running Go server. Adding a migrated route is additive: register it here, and it stops being
//! forwarded. Nothing has to be removed from a list of exclusions, because there is no list —
//! the proxy is the fallback.

pub mod auth;
pub mod error;
pub mod preferences;
pub mod proxy;
pub mod sessions;
pub mod teams;
pub mod users;

use axum::Router;
use axum::routing::{MethodRouter, get, put};
use mm_app::App;

/// Go's `PrivacySettings.ShowFullName` default (model/config.go).
pub const DEFAULT_SHOW_FULL_NAME: bool = true;
/// Go's `PrivacySettings.ShowEmailAddress` default (model/config.go).
pub const DEFAULT_SHOW_EMAIL_ADDRESS: bool = true;

/// Shared state. Cloned per request, so every field is cheap to clone — `reqwest::Client` and
/// `PgPool` are both handles over shared internals.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub http: reqwest::Client,
    /// Base URL of the Go server, without a trailing slash.
    pub go_upstream: String,
    /// Stand-ins for `ServiceSettings.PrivacySettings` until config is ported (D-085).
    pub show_full_name: bool,
    pub show_email_address: bool,
}

impl std::fmt::Debug for AppState {
    // `App` and `Client` have no useful Debug output and `go_upstream` is the field an operator
    // actually wants to see in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("go_upstream", &self.go_upstream)
            .field("show_full_name", &self.show_full_name)
            .field("show_email_address", &self.show_email_address)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn new(app: App, go_upstream: String) -> Self {
        Self {
            app,
            http: reqwest::Client::new(),
            go_upstream: go_upstream.trim_end_matches('/').to_owned(),
            show_full_name: DEFAULT_SHOW_FULL_NAME,
            show_email_address: DEFAULT_SHOW_EMAIL_ADDRESS,
        }
    }
}

/// Mark a path as partially migrated: the methods registered here are served locally, and
/// **every other method on that path is forwarded**.
///
/// This is not optional, and forgetting it is silent. axum matches the path first: once any
/// method is registered for `/api/v4/users/me/preferences`, a `GET` to that path returns **405**
/// from our router instead of reaching `Router::fallback`. Migrating `PUT` would therefore break
/// the `GET` that was previously proxied and working — measured, not theorised: the first write
/// route did exactly that, and a parity test caught it as an empty response body.
///
/// So every migrated path goes through here rather than being registered directly.
fn partially_migrated(methods: MethodRouter<AppState>) -> MethodRouter<AppState> {
    methods.fallback(proxy::forward_to_go)
}

/// Build the router.
///
/// The migrated routes are listed explicitly and everything else falls through to the proxy —
/// both unregistered paths (`Router::fallback`) and unmigrated methods on registered paths
/// (`partially_migrated`).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/v4/users/me",
            partially_migrated(get(users::get_user_me)),
        )
        .route(
            "/api/v4/users/me/sessions",
            partially_migrated(get(sessions::get_sessions_me)),
        )
        .route(
            "/api/v4/users/me/preferences",
            partially_migrated(put(preferences::update_preferences_me)),
        )
        .route(
            "/api/v4/users/me/teams/members",
            partially_migrated(get(teams::get_team_members_for_user_me)),
        )
        .fallback(proxy::forward_to_go)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `connect_lazy` still needs a reactor to exist, so this is a tokio test even though it
    // never opens a connection.
    #[tokio::test]
    async fn upstream_url_never_keeps_a_trailing_slash() {
        // The proxy concatenates `go_upstream` with a path that always starts with `/`, so a
        // trailing slash here would produce `//api/v4/...` — which some routers treat as a
        // different path than the client asked for.
        let app = App::new(mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://x/y")
                .expect("a lazy pool needs no server"),
        ));
        let state = AppState::new(app, "http://localhost:8065/".to_owned());
        assert_eq!(state.go_upstream, "http://localhost:8065");
    }
}
