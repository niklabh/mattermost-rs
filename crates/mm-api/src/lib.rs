//! REST API and the Strangler Fig proxy.
//!
//! The router serves the routes that have been migrated and forwards everything else to the still
//! running Go server. Adding a migrated route is additive: register it here, and it stops being
//! forwarded. Nothing has to be removed from a list of exclusions, because there is no list —
//! the proxy is the fallback.

pub mod auth;
pub mod channels;
pub mod error;
pub mod preferences;
pub mod proxy;
pub mod sessions;
pub mod teams;
pub mod users;

use axum::Router;
use axum::extract::{RawPathParams, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{MethodRouter, get};
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

/// Go's path-parameter charset: `{channel_id:[A-Za-z0-9]+}` (api4/api.go, 91 occurrences).
///
/// A segment outside that class never matches the route, so gorilla/mux answers its own 404 —
/// `api.context.404.app_error`, from the mux `NotFoundHandler` — before any handler runs. axum's
/// `{name}` matches the whole segment, so without this the same request reaches our handler and
/// gets a 400 from `IsValidId`: a different status, a different error id and a different body,
/// on a request Go never routed. See [D-150].
fn segment_matches_go_mux(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Which path parameters that charset applies to.
///
/// Every `*_id` parameter in api4 is `[A-Za-z0-9]+` **except `plugin_id`**, which additionally
/// allows `_`, `-` and `.` — checked across all 21 distinct `_id` patterns, not assumed. Nothing
/// here registers a plugin route yet; the exception is named so that adding one does not silently
/// inherit the wrong rule.
fn parameter_is_id_shaped(name: &str) -> bool {
    name.ends_with("_id") && name != "plugin_id"
}

/// Forward to Go any request whose id-shaped path segments Go's router would not have matched.
///
/// Layered on the parameterised routes rather than folded into each handler, because the decision
/// is "would Go have routed this at all" — which is a router question, and because forwarding
/// needs the whole `Request`, which a handler has already had extracted out from under it.
///
/// Forwarding rather than reproducing Go's 404 body is deliberate: it makes the answer Go's own,
/// including the `detailed_error` that interpolates the request URL, with nothing to keep in step.
async fn mux_segments_or_forward(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Response {
    for (name, value) in &params {
        if parameter_is_id_shaped(name) && !segment_matches_go_mux(value) {
            tracing::debug!(
                parameter = name,
                "path segment is outside Go's mux charset; forwarding so Go answers its own 404"
            );
            return proxy::forward_to_go(State(state), request).await;
        }
    }
    next.run(request).await
}

/// [`partially_migrated`], plus the segment charset check for a route with path parameters.
fn partially_migrated_with_ids(
    state: &AppState,
    methods: MethodRouter<AppState>,
) -> MethodRouter<AppState> {
    partially_migrated(methods).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        mux_segments_or_forward,
    ))
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
        // The parameterised sibling. `/users/me` above wins as a literal; every *other* literal
        // Go owns under /users (`stats`, `known`, `autocomplete`, `tokens`, …) lands here and is
        // forwarded by the handler's serve-only-exact-ids rule — see `users::get_user`.
        .route(
            "/api/v4/users/{user_id}",
            partially_migrated_with_ids(&state, get(users::get_user)),
        )
        // Deeper than the `{user_id}` route, so no conflict — and the parameter is *not*
        // id-shaped: Go's username class allows `_`, `-` and `.`, so the id-charset middleware
        // must not apply. The handler carries its own mux-charset forward instead.
        .route(
            "/api/v4/users/username/{username}",
            partially_migrated(get(users::get_user_by_username)),
        )
        .route(
            "/api/v4/users/me/sessions",
            partially_migrated(get(sessions::get_sessions_me)),
        )
        .route(
            "/api/v4/users/me/preferences",
            partially_migrated(
                get(preferences::get_preferences_me).put(preferences::update_preferences_me),
            ),
        )
        // The parameterised sibling of the literal above; `me` under the two deeper paths has no
        // literal route, so it arrives here as a value the handler resolves.
        .route(
            "/api/v4/users/{user_id}/preferences",
            partially_migrated_with_ids(&state, get(preferences::get_preferences)),
        )
        // `{category}` and `{preference_name}` are not id-shaped, so the id-charset middleware
        // leaves them alone; the handlers carry Go's own `[A-Za-z0-9_]+` mux class instead.
        .route(
            "/api/v4/users/{user_id}/preferences/{category}",
            partially_migrated_with_ids(&state, get(preferences::get_preferences_by_category)),
        )
        .route(
            "/api/v4/users/{user_id}/preferences/{category}/name/{preference_name}",
            partially_migrated_with_ids(
                &state,
                get(preferences::get_preference_by_category_and_name),
            ),
        )
        .route(
            "/api/v4/users/me/teams/members",
            partially_migrated(get(teams::get_team_members_for_user_me)),
        )
        // The parameterised sibling of the literal `me` routes above. axum matches literals
        // first, so `/users/me/teams/members` keeps hitting the route above while `me` here is
        // an ordinary value the handler resolves — the same alias rule as everywhere else.
        .route(
            "/api/v4/users/{user_id}/teams",
            partially_migrated_with_ids(&state, get(teams::get_teams_for_user)),
        )
        // Sibling literal segments under /teams/ (`/teams/name/...`, `/teams/search`, …) are
        // POST-only or deeper paths in Go; the same reasoning as the /channels/{channel_id}
        // route below applies unchanged.
        .route(
            "/api/v4/teams/{team_id}",
            partially_migrated_with_ids(&state, get(teams::get_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/stats",
            partially_migrated_with_ids(&state, get(teams::get_team_stats)),
        )
        // Sibling literal segments (`/channels/direct`, `/channels/search`, …) are all POST-only
        // in Go, and all alphanumeric. A GET to one of them matches `{channel_id}` here exactly
        // as it matches gorilla's `{channel_id:[A-Za-z0-9]+}` there, and 400s identically; a POST
        // falls to `partially_migrated`'s method fallback and is forwarded. A literal segment
        // with a hyphen would land on `mux_segments_or_forward` and be forwarded too.
        .route(
            "/api/v4/channels/{channel_id}",
            partially_migrated_with_ids(&state, get(channels::get_channel)),
        )
        // Go's sibling `POST /channels/stats/member_count` (api.go:60) never lands here: its
        // last segment is `member_count`, not `stats`, so it falls to `Router::fallback` and is
        // forwarded whole.
        .route(
            "/api/v4/channels/{channel_id}/stats",
            partially_migrated_with_ids(&state, get(channels::get_channel_stats)),
        )
        .route(
            "/api/v4/channels/{channel_id}/members",
            partially_migrated_with_ids(&state, get(channels::get_channel_members)),
        )
        // The first migrated path with parameters. axum's `{name}` segments bind by position in
        // the handler's `Path` tuple, so the order here is the order there.
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}",
            partially_migrated_with_ids(&state, get(channels::get_channel_member)),
        )
        // Note the segment order: the **user** comes first here and second above, because Go
        // hangs this one off `BaseRoutes.ChannelForUser` (api4/api.go:223). The handler's `Path`
        // tuple has to match this, not the other route's.
        .route(
            "/api/v4/users/{user_id}/channels/{channel_id}/unread",
            partially_migrated_with_ids(&state, get(channels::get_channel_unread)),
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
