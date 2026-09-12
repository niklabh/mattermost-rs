//! REST API and the Strangler Fig proxy.
//!
//! The router serves the routes that have been migrated and forwards everything else to the still
//! running Go server. Adding a migrated route is additive: register it here, and it stops being
//! forwarded. Nothing has to be removed from a list of exclusions, because there is no list —
//! the proxy is the fallback.

pub mod audits;
pub mod auth;
pub mod auth_writes;
/// The two bot reads. `getBot` and `getBots`.
pub mod bots;
pub mod channel_creates;
pub mod channel_member_writes;
pub mod channel_writes;
pub mod channels;
pub mod cloud;
pub mod commands;
pub mod common_teams;
pub mod compliance;
pub mod connected_workspaces;
pub mod custom_profile_attributes;
pub mod data_retention;
pub mod drafts;
pub mod emoji;
pub mod error;
/// The four export routes and the two import ones.
pub mod exports;
pub mod feature_gates;
/// `getFileInfo` — the one `/files/` route that returns JSON rather than bytes.
/// Ten reads that refuse before they read anything. One module, eight `api4` files.
pub mod gated_reads;

/// Port of `api4/view.go` — the seven integrated-boards routes.
pub mod channel_join_requests;
pub mod files;
pub mod groups;
/// The four routes that answer with a stored image: profile, team icon, emoji, brand.
pub mod images;
/// The three job reads. `getJobs`, `getJob` and `getJobsByType`.
pub mod jobs;
pub mod license;
pub mod licensed_features;
pub mod limits;
/// The local-mode admin API: the api4 handlers on a unix socket, with an unrestricted session.
pub mod local;
pub mod oauth;
pub mod permissions;
pub mod post_writes;
pub mod posts;
pub mod preferences;
pub mod properties;
pub mod proxy;
pub mod reactions;
pub mod recaps;
pub mod reports;
pub mod roles;
pub mod schemes;
/// Port of `web.WriteFileResponse` and the `http.ServeContent` behind it.
pub mod serve_content;
pub mod sessions;
pub mod sidebar;
pub mod status;
pub mod system;
pub mod team_member_writes;
pub mod teams;
pub mod terms_of_service;
/// The thread write family: `PUT …/threads/read` and the two `/following` methods.
pub mod thread_writes;
/// The four personal-access-token reads.
pub mod tokens;
/// The two upload-session reads.
pub mod uploads;
pub mod usage;
pub mod users;
pub mod views;
pub mod webhooks;
/// `GET /api/v4/websocket` — the upgrade, the pumps, and the action router.
pub mod websocket;

/// The three `/api/v4/config` reads. Appended rather than filed alphabetically, because this
/// list is shared by every worktree and a middle insertion is somebody else's merge conflict.
pub mod config;

use axum::Router;
use axum::extract::{RawPathParams, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use mm_app::App;

/// Shared state. Cloned per request, so every field is cheap to clone — `reqwest::Client` and
/// `PgPool` are both handles over shared internals.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub http: reqwest::Client,
    /// Base URL of the Go server, without a trailing slash.
    pub go_upstream: String,
}

impl std::fmt::Debug for AppState {
    // `App` and `Client` have no useful Debug output and `go_upstream` is the field an operator
    // actually wants to see in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("go_upstream", &self.go_upstream)
            .field("show_full_name", &self.show_full_name())
            .field("show_email_address", &self.show_email_address())
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn new(app: App, go_upstream: String) -> Self {
        Self {
            app,
            http: reqwest::Client::new(),
            go_upstream: go_upstream.trim_end_matches('/').to_owned(),
        }
    }

    /// `PrivacySettings.ShowFullName`, read through to the app's configuration.
    ///
    /// An accessor rather than a field on purpose. These were `AppState` fields hardcoded to
    /// Go's defaults while there was no configuration to read ([D-085]); now that
    /// [`mm_app::config::Config`] loads the document the Go server persists, a copy on `AppState`
    /// would be a second source of truth that a reload could leave stale. There is exactly one
    /// place either value lives.
    pub fn show_full_name(&self) -> bool {
        self.app.config().show_full_name
    }

    /// `PrivacySettings.ShowEmailAddress`. See [`AppState::show_full_name`].
    pub fn show_email_address(&self) -> bool {
        self.app.config().show_email_address
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

/// Whether Go's mux would have matched this segment against the pattern it carries for this
/// parameter name.
///
/// Two api4 parameters are not `_id`-shaped and carry their own classes, both in
/// `api4/properties.go`'s prefixes (api.go:346-350):
///
/// * `{group_name:[a-z][a-z0-9_]*}` — lower-case, digits and underscores, **never leading with a
///   digit or an underscore**. `Boards` and `_x` are both mux 404s where `IsValidPropertyGroupName`
///   would have given a 400.
/// * `{object_type:[a-z]+}` — lower-case letters only, so `XY` is a 404 and `xyz` reaches the
///   handler's `RequireObjectType` for its 400. Both measured.
///
/// A parameter this does not know about is not checked, which is the right default: axum's
/// `{name}` already matches one whole segment, and every pattern in api4 is a subset of that.
fn segment_matches_go_mux_for(name: &str, value: &str) -> bool {
    match name {
        "group_name" => {
            let mut bytes = value.bytes();
            bytes.next().is_some_and(|b| b.is_ascii_lowercase())
                && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }
        "object_type" => !value.is_empty() && value.bytes().all(|b| b.is_ascii_lowercase()),
        _ if parameter_is_id_shaped(name) => segment_matches_go_mux(value),
        _ => true,
    }
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
        if !segment_matches_go_mux_for(name, value) {
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

/// Port of the security headers `web.Handler.ServeHTTP` sets on **every** API response
/// (web/handlers.go:242) and of the `Vary` that `gzhttp.GzipHandler` adds around it.
///
/// # This was missing from every migrated route, and no test could see it
///
/// Go sets these before the handler runs, so they are on the wire for all 285 route+method pairs
/// this server answers — and until the file-bytes suite, **no parity test compared response
/// headers at all**. `fetch_both` asserts bodies. So 264 pairs shipped without
/// `Referrer-Policy`, `Permissions-Policy` or `Expires`, byte-identical in the body and
/// materially different on the wire. See [D-207].
///
/// # Only our own responses
///
/// Guarded on `x-mmrs-served-by`, which every locally-served response carries and no proxied one
/// does. A forwarded response already has Go's own headers, including the two per-request ones
/// this cannot mint (`X-Request-Id`, `X-Version-Id`).
///
/// # Three details
///
/// - **`Permissions-Policy` is the empty string**, deliberately (Go's comment calls these
///   "hardcoded sensible default values"). An empty header value is legal and is what Go sends.
/// - **`Expires: 0` is `GET` only.** Go's guard is `if r.Method == "GET"`, so a `HEAD` on the
///   same route has no `Expires` — which axum's GET/HEAD dispatch makes easy to get wrong, since
///   the *handler* cannot tell the difference by then.
/// - **`Vary: Accept-Encoding` comes from the gzip wrapper**, not from the handler, so it is
///   present exactly when `WebserverMode` is `gzip` — the default. What this port does *not*
///   reproduce is the compression itself: a client sending `Accept-Encoding: gzip` gets a
///   compressed body from Go and an uncompressed one from us. See [D-208].
///
/// `Strict-Transport-Security` is not reproduced: it is gated on `TLSStrictTransport`, which
/// defaults to `false` and is not modelled in [`mm_app::config::Config`].
pub(crate) async fn go_global_headers(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let is_get = request.method() == axum::http::Method::GET;
    let gzip_mode = state.app.config().webserver_mode == "gzip";

    let mut response = next.run(request).await;

    // A forwarded response already carries Go's own headers.
    if !response.headers().contains_key("x-mmrs-served-by") {
        return response;
    }

    let headers = response.headers_mut();
    headers.insert(
        axum::http::HeaderName::from_static("permissions-policy"),
        axum::http::HeaderValue::from_static(""),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("referrer-policy"),
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    if is_get {
        headers.insert(
            axum::http::header::EXPIRES,
            axum::http::HeaderValue::from_static("0"),
        );
    }
    if gzip_mode {
        headers.insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Accept-Encoding"),
        );
    }

    response
}

/// The 400 `/api/v4/posts/{post_id}` answers for a segment that is not a 26-character id.
///
/// Registered on the `GET`, `PUT` and `DELETE` of the **literal** `/api/v4/posts/ephemeral` so
/// that adding that literal does not take those three methods away from `posts::get_post`,
/// `post_writes::update_post` and `post_writes::delete_post` — each of which refuses the
/// nine-character segment with exactly this error, before reading a body or touching the
/// database. axum prefers a static segment over `{param}` and does not fall back across method
/// routers, so without this the three would silently start being forwarded. See [D-330].
async fn invalid_post_id_param() -> axum::response::Response {
    axum::response::IntoResponse::into_response(crate::error::ApiError::invalid_url_param(
        "post_id",
    ))
}

/// Build the router.
///
/// The migrated routes are listed explicitly and everything else falls through to the proxy —
/// both unregistered paths (`Router::fallback`) and unmigrated methods on registered paths
/// (`partially_migrated`).
pub fn router(state: AppState) -> Router {
    Router::new()
        // `api.BaseRoutes.APIRoot.Handle("/{websocket:websocket(?:\\/)?}")` (api4/websocket.go:52)
        // — the gorilla pattern accepts a trailing slash, so both spellings are registered. This
        // is the one route the proxy could never forward: `forward_to_go` strips `Connection` and
        // `Upgrade` as hop-by-hop headers, so a client pointed at this server had no socket at
        // all before it was served here.
        .route("/api/v4/websocket", get(websocket::connect_websocket))
        .route("/api/v4/websocket/", get(websocket::connect_websocket))
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
        // The literal `ids` beside `{user_id}`: axum prefers the literal, so `POST /users/ids`
        // lands here while `GET /users/ids` is forwarded by `partially_migrated` and Go
        // answers as before. The GET route's exact-26-char rule never saw `ids` anyway (three
        // characters), so nothing that was forwarded stops being forwarded.
        // `BaseRoutes.Users.Handle("/group_channels")` (api4/user.go:39), POST-only — another
        // literal sibling of `{user_id}`, same precedence reasoning as `ids` below.
        .route(
            "/api/v4/users/group_channels",
            partially_migrated(post(users::get_users_by_group_channel_ids)),
        )
        // `BaseRoutes.Users.Handle("/search")` (api4/user.go:35) — the add-members dialog.
        .route(
            "/api/v4/users/search",
            partially_migrated(post(users::search_users)),
        )
        // `BaseRoutes.Users.Handle("/usernames")` (api4/user.go:33) — the webapp posts the
        // usernames it found in a page of posts.
        .route(
            "/api/v4/users/usernames",
            partially_migrated(post(users::get_users_by_names)),
        )
        .route(
            "/api/v4/users/ids",
            partially_migrated(post(users::get_users_by_ids)),
        )
        // The same precedence question as `ids` above, with the same answer. `autocomplete` is
        // a literal sibling of `{user_id}`; axum prefers the literal, so `GET
        // /users/autocomplete` lands here. Nothing that used to be answered stops being
        // answered: the `{user_id}` handler serves only exact 26-character ids and forwarded
        // this twelve-character segment to Go — which is what the comment on that route already
        // says, naming `autocomplete` among the literals it hands on. Registered GET-only, so
        // any other method on the path falls to `partially_migrated`'s fallback and is
        // forwarded exactly as before.
        .route(
            "/api/v4/users/autocomplete",
            partially_migrated(get(users::autocomplete_users)),
        )
        // Deeper than the `{user_id}` route, so no conflict — and the parameter is *not*
        // id-shaped: Go's username class allows `_`, `-` and `.`, so the id-charset middleware
        // must not apply. The handler carries its own mux-charset forward instead.
        // `BaseRoutes.UserByEmail` (api4/api.go:205) — `PathPrefix("/email/{email:.+}")`, whose
        // `.+` matches slashes, so this is a wildcard rather than one segment. `POST
        // /users/email/verify` is registered *before* it in Go and wins there; here the method
        // does the same job, since only GET is served.
        .route(
            "/api/v4/users/email/{*email}",
            partially_migrated(get(users::get_user_by_email)),
        )
        .route(
            "/api/v4/users/username/{username}",
            partially_migrated(get(users::get_user_by_username)),
        )
        // Was the literal `/users/me/sessions`; now the parameterised route, with `me` resolved
        // in the handler like every other alias. The `me` bytes are unchanged — pinned by the
        // parity suite — and the gate that was `true` by construction is now evaluated.
        // `BaseRoutes.PostsForUser.Handle("/flagged")` (api4/post.go:35) —
        // `/users/{user_id}/posts/flagged`. Two segments deeper than `/users/{user_id}`, so it
        // shadows nothing; `partially_migrated_with_ids` keeps the exact-26-char rule the
        // handler's own `RequireUserId` would otherwise have to answer for.
        // `BaseRoutes.User.Handle("/channel_members")` (api4/user.go:107) — one segment deeper
        // than `/users/{user_id}`, so it shadows nothing.
        // `BaseRoutes.ThreadsForUser` (api4/api.go) — `/users/{id}/teams/{id}/threads`, three
        // segments deeper than `/users/{user_id}` and a sibling of the team channel lists.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/threads",
            partially_migrated_with_ids(&state, get(users::get_threads_for_user)),
        )
        // `BaseRoutes.UserThread` (api4/api.go) — one segment deeper than the thread list.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}",
            partially_migrated_with_ids(&state, get(users::get_thread_for_user)),
        )
        // `BaseRoutes.UserThreads.Handle("/read")` (api4/user.go:112), PUT only.
        //
        // A **static** sibling of `{thread_id}`, and matchit has no method dimension in its path
        // preference — so this route wins for every method on `/threads/read`, including the
        // `GET` that gorilla falls through to `getThreadForUser` with `read` as the thread id.
        // That is why the fallback matters: an unregistered method here is forwarded, and Go
        // answers its own 400 from the route it would have reached.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/threads/read",
            partially_migrated_with_ids(
                &state,
                put(thread_writes::update_read_state_all_threads_by_user),
            ),
        )
        // `BaseRoutes.UserThread.Handle("/following")` (api4/user.go:115-116) — PUT and DELETE
        // on one path, two handlers in Go and two here.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following",
            partially_migrated_with_ids(
                &state,
                put(thread_writes::follow_thread_by_user)
                    .delete(thread_writes::unfollow_thread_by_user),
            ),
        )
        // `BaseRoutes.TeamForUser.Handle("/drafts")` (api4/drafts.go:17) — the threads routes'
        // sibling under the same base, and the one route here whose `{user_id}` is decorative.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/drafts",
            partially_migrated_with_ids(&state, get(drafts::get_drafts)),
        )
        .route(
            "/api/v4/users/{user_id}/channel_members",
            partially_migrated_with_ids(&state, get(users::get_channel_members_for_user)),
        )
        .route(
            "/api/v4/users/{user_id}/posts/flagged",
            partially_migrated_with_ids(&state, get(posts::get_flagged_posts_for_user)),
        )
        .route(
            "/api/v4/users/{user_id}/sessions",
            partially_migrated_with_ids(&state, get(sessions::get_sessions)),
        )
        // The session write family (api4/user.go:82-85). Three segments under `/users/`, so all
        // three are siblings of `/users/{user_id}/sessions` rather than shadowing it, and the
        // literal `sessions` under the parameterised `{user_id}` keeps `revoke` from colliding
        // with anything else.
        .route(
            "/api/v4/users/{user_id}/sessions/revoke",
            partially_migrated_with_ids(&state, post(sessions::revoke_session)),
        )
        .route(
            "/api/v4/users/{user_id}/sessions/revoke/all",
            partially_migrated_with_ids(&state, post(sessions::revoke_all_sessions_for_user)),
        )
        // `BaseRoutes.Users`, not `.User` — the literal `sessions` here is a sibling of
        // `{user_id}` and axum prefers it, so `POST /users/sessions/revoke/all` lands on the
        // all-users handler rather than being read as a user called `sessions`. Go's router makes
        // the same choice, by registration order.
        .route(
            "/api/v4/users/sessions/revoke/all",
            partially_migrated(post(sessions::revoke_all_sessions_all_users)),
        )
        .route(
            "/api/v4/users/sessions/device",
            partially_migrated(put(sessions::handle_device_props)),
        )
        // `BaseRoutes.Users.Handle("/logout", APIHandler(logout))` (api4/user.go:75) — a literal
        // sibling of `{user_id}`, so axum's static-first preference lands `POST /users/logout`
        // here while the `{user_id}` route keeps every other method and every other segment. The
        // handler needs no session, which is why it takes `OptionalSession` rather than being
        // registered behind the extractor that 401s.
        .route(
            "/api/v4/users/logout",
            partially_migrated(post(auth_writes::logout)),
        )
        // `BaseRoutes.Users.Handle("/password/reset", APIHandler(resetPassword))`
        // (api4/user.go:55). Two segments under `/users/`, so it is a sibling of
        // `/users/{user_id}/{anything}` and the literal `password` wins over the parameter.
        //
        // Its neighbour `/password/reset/send` (user.go:56) is **not** registered: it exists only
        // to send an e-mail, and there is no e-mail service here. Leaving it unregistered is what
        // keeps it forwarded — adding it as a 405-only path would break it. See [D-238].
        .route(
            "/api/v4/users/password/reset",
            partially_migrated(post(auth_writes::reset_password)),
        )
        // `BaseRoutes.Users.Handle("/email/verify", APIHandler(verifyUserEmail))`
        // (api4/user.go:57), and the one registration on this list with a real routing subtlety.
        //
        // `/api/v4/users/email/{*email}` is already registered above as a **catch-all** — Go's
        // `PathPrefix("/email/{email:.+}")`, whose `.+` matches slashes. matchit prefers a static
        // segment to a catch-all, so `POST /users/email/verify` lands here rather than being read
        // as an e-mail address of `verify`. That is also Go's outcome, reached differently: there
        // the literal route is registered *first* and gorilla matches in order.
        //
        // `GET /users/email/verify` falls to this route's `partially_migrated` fallback and is
        // forwarded, so Go still answers it as a lookup for the address `verify` — unchanged.
        //
        // `/email/verify/send` (user.go:58) is unregistered for the same reason as
        // `/password/reset/send`: it is an e-mail and nothing else.
        //
        // `GET` on this path is **not** forwarded: matchit has no method dimension in its path
        // preference, so the static route wins for every method, and Go's gorilla *does* fall
        // through to the catch-all for a GET. `get_user_by_email_verify` hands it to the handler
        // Go would have reached, with `verify` as the address.
        .route(
            "/api/v4/users/email/verify",
            partially_migrated(
                post(auth_writes::verify_user_email).get(auth_writes::get_user_by_email_verify),
            ),
        )
        // `BaseRoutes.User.Handle("/password", APISessionRequired(updatePassword))`
        // (api4/user.go:51), PUT only — a sibling of `/users/{user_id}/status` and one segment
        // deeper than `/users/{user_id}`, so it shadows nothing.
        .route(
            "/api/v4/users/{user_id}/password",
            partially_migrated_with_ids(&state, put(auth_writes::update_password)),
        )
        // `BaseRoutes.User.Handle("/reset_failed_attempts", ...)` (api4/user.go:62), POST only.
        .route(
            "/api/v4/users/{user_id}/reset_failed_attempts",
            partially_migrated_with_ids(&state, post(auth_writes::reset_password_failed_attempts)),
        )
        .route(
            "/api/v4/users/{user_id}/status",
            partially_migrated_with_ids(
                &state,
                get(status::get_user_status).put(status::update_user_status),
            ),
        )
        .route(
            "/api/v4/users/{user_id}/status/custom",
            partially_migrated_with_ids(
                &state,
                put(status::update_user_custom_status).delete(status::remove_user_custom_status),
            ),
        )
        .route(
            "/api/v4/users/{user_id}/status/custom/recent",
            partially_migrated_with_ids(
                &state,
                axum::routing::delete(status::remove_user_recent_custom_status),
            ),
        )
        // Gorilla registers `removeUserRecentCustomStatus` twice; the POST form exists for
        // clients that cannot put a body on a DELETE, and reads one just the same.
        .route(
            "/api/v4/users/{user_id}/status/custom/recent/delete",
            partially_migrated_with_ids(&state, post(status::remove_user_recent_custom_status)),
        )
        // A literal under `/users/` whose *second* segment is `status` — it cannot collide with
        // `/users/{user_id}/status` above (third segment `ids` vs `status`), and axum prefers
        // the literal anyway. Registered as POST only: a GET here is forwarded and Go answers
        // its own mux 404 (measured — not a 405), exactly as before this route existed.
        .route(
            "/api/v4/users/status/ids",
            partially_migrated(post(status::get_user_statuses_by_ids)),
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
        // `BaseRoutes.Preferences.Handle("/delete")` (api4/preference.go:19). A literal sibling
        // of `{category}` below, so axum's literal-first precedence puts it here; the `{category}`
        // route never saw `delete` as a category on Go either, because Go registers this path
        // explicitly.
        .route(
            "/api/v4/users/{user_id}/preferences/delete",
            partially_migrated_with_ids(
                &state,
                post(preferences::delete_preferences)
                    // gorilla falls a GET past the POST-only `/delete` route onto `{category}`;
                    // axum does not fall through, so the literal route has to answer it too.
                    .get(preferences::get_preferences_named_delete),
            ),
        )
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
        // `BaseRoutes.TeamsForUser` (api.go:33): the literal `unread` sits beside `{team_id}`
        // routes below; axum prefers the literal, gorilla registers it first — same answer.
        .route(
            "/api/v4/users/{user_id}/teams/unread",
            partially_migrated_with_ids(&state, get(teams::get_teams_unread_for_user)),
        )
        // Sibling literal segments under /teams/ (`/teams/name/...`, `/teams/search`, …) are
        // POST-only or deeper paths in Go; the same reasoning as the /channels/{channel_id}
        // route below applies unchanged.
        .route(
            "/api/v4/teams/{team_id}",
            partially_migrated_with_ids(
                &state,
                get(teams::get_team)
                    .put(teams::update_team)
                    .delete(teams::delete_team),
            ),
        )
        // `BaseRoutes.Team.Handle("/patch")` (api4/team.go) — a segment deeper than `{team_id}`,
        // so no precedence question with the route above.
        .route(
            "/api/v4/teams/{team_id}/patch",
            partially_migrated_with_ids(&state, axum::routing::put(teams::patch_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/restore",
            partially_migrated_with_ids(&state, post(teams::restore_team)),
        )
        // `BaseRoutes.Team.Handle("/privacy")` (api4/team.go:46) — PUT only, one segment deeper
        // than `{team_id}`.
        .route(
            "/api/v4/teams/{team_id}/privacy",
            partially_migrated_with_ids(&state, axum::routing::put(teams::update_team_privacy)),
        )
        .route(
            "/api/v4/teams/{team_id}/regenerate_invite_id",
            partially_migrated_with_ids(&state, post(teams::regenerate_team_invite_id)),
        )
        .route(
            "/api/v4/teams/{team_id}/stats",
            partially_migrated_with_ids(&state, get(teams::get_team_stats)),
        )
        // `team_name` is not id-shaped — Go's class is `[A-Za-z0-9_-]+` — so the id-charset
        // middleware must not apply; the handler carries its own mux forward, like `username`.
        // axum gives the static `name` precedence over `{team_id}` above, which is the *reverse*
        // of gorilla's registration order; the handler forwards the three GET literals that
        // order would have sent to `{team_id}` (see `teams::TEAM_BY_NAME_SHADOWED_LITERALS`).
        .route(
            "/api/v4/teams/name/{team_name}",
            partially_migrated(get(teams::get_team_by_name)),
        )
        // `BaseRoutes.ChannelByNameForTeamName` (api.go:225) — two segments deeper than
        // `/teams/name/{team_name}` above, and neither segment is id-shaped (`[A-Za-z0-9_-]+`
        // for both), so the id middleware stays off and the handler carries both mux classes
        // forward itself.
        // `BaseRoutes.TeamByName.Handle("/exists")` (api4/team.go:68).
        .route(
            "/api/v4/teams/name/{team_name}/exists",
            partially_migrated(get(teams::team_exists)),
        )
        .route(
            "/api/v4/teams/name/{team_name}/channels/name/{channel_name}",
            partially_migrated(get(channels::get_channel_by_name_for_team_name)),
        )
        // gorilla registers the GET and the POST separately on `BaseRoutes.TeamMembers`
        // (api4/team.go:56, :59) and picks by method; chaining onto one `MethodRouter`
        // reproduces that, and axum panics on a second `.route()` for the same path.
        .route(
            "/api/v4/teams/{team_id}/members",
            partially_migrated_with_ids(
                &state,
                get(teams::get_team_members).post(team_member_writes::add_team_member),
            ),
        )
        // `BaseRoutes.TeamMembers.Handle("/batch")` (api4/team.go:61). The literal wins over the
        // `{user_id}` pattern below in both routers, so `/members/batch` never reaches
        // `getTeamMember` — the same shape as `/members/ids`.
        .route(
            "/api/v4/teams/{team_id}/members/batch",
            partially_migrated_with_ids(&state, post(team_member_writes::add_team_members)),
        )
        // `BaseRoutes.TeamMembers.Handle("/ids")` (api4/team.go:57) — the same literal-in-the-
        // parameter-slot shape as `/channels/{id}/members/ids` above, with the same answer in
        // both routers and for the same reason: POST-only here and there, so a GET keeps
        // reaching `getTeamMember` with `user_id = "ids"` and its 400.
        .route(
            "/api/v4/teams/{team_id}/members/ids",
            partially_migrated_with_ids(&state, post(teams::get_team_members_by_ids)),
        )
        .route(
            "/api/v4/teams/{team_id}/members/{user_id}",
            partially_migrated_with_ids(
                &state,
                get(teams::get_team_member).delete(team_member_writes::remove_team_member),
            ),
        )
        // `BaseRoutes.TeamMember.Handle("/roles")` and `.Handle("/schemeRoles")`
        // (api4/team.go:69-70). **`schemeRoles` is camelCase**, matched literally by gorilla and
        // by axum alike, so `/schemeroles` reaches neither and forwards to Go's own 404.
        .route(
            "/api/v4/teams/{team_id}/members/{user_id}/roles",
            partially_migrated_with_ids(&state, put(team_member_writes::update_team_member_roles)),
        )
        .route(
            "/api/v4/teams/{team_id}/members/{user_id}/schemeRoles",
            partially_migrated_with_ids(
                &state,
                put(team_member_writes::update_team_member_scheme_roles),
            ),
        )
        // `team_id` gets the id-charset middleware; `channel_name` is not id-shaped and Go's
        // class for it is `[A-Za-z0-9_-]+`, so the handler carries its own mux forward. Go's
        // `/teams/name/{team_name}/channels/name/{channel_name}` sibling has one more segment
        // and never lands here.
        .route(
            "/api/v4/teams/{team_id}/channels/name/{channel_name}",
            partially_migrated_with_ids(&state, get(channels::get_channel_by_name)),
        )
        // `BaseRoutes.TeamForUser` (api.go:34). Its deeper siblings `…/channels/members` and
        // `…/channels/categories` are their own routes below — one segment longer, so nothing
        // here shadows them.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/channels",
            partially_migrated_with_ids(&state, get(channels::get_channels_for_team_for_user)),
        )
        // `BaseRoutes.ChannelMembersForUser` (api.go:229), a subrouter with one GET at "".
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/channels/members",
            partially_migrated_with_ids(
                &state,
                get(channels::get_channel_members_for_team_for_user),
            ),
        )
        // `BaseRoutes.TeamForUser` again (api.go:34): the singular unread, a sibling of
        // `/users/{user_id}/teams/unread` above with one more segment. Nothing shadows it — the
        // literal `unread` is the last segment here, not the `{team_id}` slot.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/unread",
            partially_migrated_with_ids(&state, get(teams::get_team_unread)),
        )
        // `BaseRoutes.TeamForUser` once more (api4/channel.go:76) — the whole team marked read.
        // A `PUT` on a static leaf beside `/unread` and `/channels`, gated on the
        // `EnableShiftEscapeToMarkAllRead` feature flag, which both servers now turn on through
        // the environment (`scripts/go-server.sh`, `scripts/mm-api-env.sh`).
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/read",
            partially_migrated_with_ids(&state, put(channels::read_all_in_team)),
        )
        // Sibling literal segments (`/channels/direct`, `/channels/search`, …) are all POST-only
        // in Go, and all alphanumeric. A GET to one of them matches `{channel_id}` here exactly
        // as it matches gorilla's `{channel_id:[A-Za-z0-9]+}` there, and 400s identically; a POST
        // falls to `partially_migrated`'s method fallback and is forwarded. A literal segment
        // with a hyphen would land on `mux_segments_or_forward` and be forwarded too.
        // `BaseRoutes.Channels.Handle("")` (api4/channel.go:41) — the only method on the bare
        // `/channels` collection that this server answers. `getAllChannels` is a `GET` on the
        // same path and is not migrated, so there is no method to combine with here yet; a `GET`
        // falls to `partially_migrated`'s method fallback and is forwarded.
        .route(
            "/api/v4/channels",
            partially_migrated(post(channel_creates::create_channel)),
        )
        // `BaseRoutes.Channels.Handle("/direct")` and `("/group")` (api4/channel.go:42, :43).
        // Two literal segments in the `{channel_id}` slot of `/channels/{channel_id}` below,
        // which matchit resolves to the static route and gorilla resolves to whichever was
        // registered first — the same one. Both are POST-only in Go.
        .route(
            "/api/v4/channels/direct",
            partially_migrated(post(channel_creates::create_direct_channel)),
        )
        .route(
            "/api/v4/channels/group",
            partially_migrated(post(channel_creates::create_group_channel)),
        )
        // `BaseRoutes.Channels.Handle("/stats/member_count")` (api4/channel.go:47) — posted
        // with the sidebar's channel ids. A literal two segments deep, so it shadows nothing.
        .route(
            "/api/v4/channels/stats/member_count",
            partially_migrated(post(channels::get_channels_member_count)),
        )
        // `BaseRoutes.Channels.Handle("/members/{user_id}/view")` and its two siblings
        // (api4/channel.go:53-55). The literal `members` sits in the `{channel_id}` slot of the
        // routes below, exactly as `stats` does above — matchit prefers the static segment, and
        // gorilla's ordered matcher picks the same registration, so both routers send
        // `/channels/members/<id>/view` here and `/channels/<id>/members/<id>` to
        // `getChannelMember`.
        //
        // `/direct/read` is a **`PUT`**, and one segment deeper than its two siblings.
        .route(
            "/api/v4/channels/members/{user_id}/view",
            partially_migrated_with_ids(&state, post(channels::view_channel)),
        )
        .route(
            "/api/v4/channels/members/{user_id}/mark_read",
            partially_migrated_with_ids(&state, post(channels::read_multiple_channels)),
        )
        .route(
            "/api/v4/channels/members/{user_id}/direct/read",
            partially_migrated_with_ids(&state, put(channels::read_all_messages)),
        )
        // Three methods on one path, because **axum panics on a duplicate route path** and Go
        // registers three separate `BaseRoutes.Channel.Handle("")` calls (api4/channel.go:85, :86,
        // :91) that gorilla's method matcher then splits. `MethodRouter::get(...).put(...)
        // .delete(...)` is the same split: a fourth method still falls to
        // `partially_migrated`'s method fallback and is forwarded, exactly as gorilla's 405 path
        // would be.
        .route(
            "/api/v4/channels/{channel_id}",
            partially_migrated_with_ids(
                &state,
                get(channels::get_channel)
                    .put(channel_writes::update_channel)
                    .delete(channel_writes::delete_channel),
            ),
        )
        // `BaseRoutes.Channel.Handle("/patch")` and `/privacy` are both **PUT**, and `/restore` is
        // a **POST** — not the DELETE/PUT symmetry the names suggest. Registered method-exactly so
        // that a wrong verb reaches Go and gets its 405 rather than ours.
        .route(
            "/api/v4/channels/{channel_id}/patch",
            partially_migrated_with_ids(&state, put(channel_writes::patch_channel)),
        )
        .route(
            "/api/v4/channels/{channel_id}/privacy",
            partially_migrated_with_ids(&state, put(channel_writes::update_channel_privacy)),
        )
        .route(
            "/api/v4/channels/{channel_id}/restore",
            partially_migrated_with_ids(&state, post(channel_writes::restore_channel)),
        )
        // Go's sibling `POST /channels/stats/member_count` (api.go:60) never lands here: its
        // last segment is `member_count`, not `stats`, so it falls to `Router::fallback` and is
        // forwarded whole.
        .route(
            "/api/v4/channels/{channel_id}/stats",
            partially_migrated_with_ids(&state, get(channels::get_channel_stats)),
        )
        // Three methods on one path. **axum panics on a duplicate route path**, so the POST and
        // PUT are chained onto the GET's `MethodRouter` rather than added as a second `.route`.
        // gorilla registers all three separately on `BaseRoutes.ChannelMembers`
        // (api4/channel.go:107-110) and picks by method, which is what chaining reproduces.
        .route(
            "/api/v4/channels/{channel_id}/members",
            partially_migrated_with_ids(
                &state,
                get(channels::get_channel_members)
                    .post(channel_member_writes::add_channel_member)
                    .put(channel_member_writes::set_channel_members),
            ),
        )
        // The first migrated path with parameters. axum's `{name}` segments bind by position in
        // the handler's `Path` tuple, so the order here is the order there.
        // The literal `ids` in the `{user_id}` slot of the route below. gorilla registers
        // `ChannelMembers.Handle("/ids")` POST-only (api4/channel.go:108), so a **GET** to this
        // path there falls through to `getChannelMember` with `user_id = "ids"` and 400s;
        // registered POST-only here, a GET lands on `partially_migrated`'s method fallback and
        // Go answers exactly that 400 as before. axum prefers the literal for POST, which is the
        // route gorilla's method matcher picks too — same answer, both methods.
        .route(
            "/api/v4/channels/{channel_id}/members/ids",
            partially_migrated_with_ids(&state, post(channels::get_channel_members_by_ids)),
        )
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}",
            partially_migrated_with_ids(
                &state,
                get(channels::get_channel_member)
                    .delete(channel_member_writes::remove_channel_member),
            ),
        )
        // The three `PUT`s one segment deeper. `BaseRoutes.ChannelMember` (api4/channel.go:113-116)
        // registers `/roles`, `/schemeRoles` and `/notify_props` as literals under the `{user_id}`
        // parameter, so neither router has a precedence puzzle here — and `/autotranslation`, the
        // fourth literal, is deliberately unregistered (it needs the AutoTranslation store) and
        // falls to `Router::fallback` whole.
        //
        // **`schemeRoles` is camelCase**, alone among these paths. gorilla matches it literally and
        // so does axum, so `/schemeroles` reaches neither and is forwarded.
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}/roles",
            partially_migrated_with_ids(
                &state,
                put(channel_member_writes::update_channel_member_roles),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}/schemeRoles",
            partially_migrated_with_ids(
                &state,
                put(channel_member_writes::update_channel_member_scheme_roles),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/members/{user_id}/notify_props",
            partially_migrated_with_ids(
                &state,
                put(channel_member_writes::update_channel_member_notify_props),
            ),
        )
        // `BaseRoutes.PostsForChannel` (api.go:240) — a `PathPrefix("/posts")` subrouter with a
        // single `GET` at `""`. gorilla's prefix router 404s anything deeper (`/posts/unread`
        // hangs off `ChannelForUser`, not off this), and axum's exact path leaves those to
        // `Router::fallback`, so both routers answer the same way for every path but this one.
        //
        // The three cursor branches (`since`, `after`, `before`) and `collapsedThreadsExtended`
        // are forwarded **by the handler**, not by the router — they are query parameters, and a
        // router cannot see them. `mm_api::posts::get_posts_for_channel` says why each goes.
        .route(
            "/api/v4/channels/{channel_id}/posts",
            partially_migrated_with_ids(&state, get(posts::get_posts_for_channel)),
        )
        // `BaseRoutes.User.Handle("/channels")` (api4/channel.go:75). Exact match: the deeper
        // `…/channels/{channel_id}/unread` below is its own route, and Go has no literal
        // sibling directly under `/users/{user_id}/channels/`.
        .route(
            "/api/v4/users/{user_id}/channels",
            partially_migrated_with_ids(&state, get(channels::get_channels_for_user)),
        )
        // Note the segment order: the **user** comes first here and second above, because Go
        // hangs this one off `BaseRoutes.ChannelForUser` (api4/api.go:223). The handler's `Path`
        // tuple has to match this, not the other route's.
        .route(
            "/api/v4/users/{user_id}/channels/{channel_id}/unread",
            partially_migrated_with_ids(&state, get(channels::get_channel_unread)),
        )
        // `BaseRoutes.ChannelForUser.Handle("/posts/unread")` (api4/post.go:37) — one segment
        // deeper than `/unread` above and its only sibling under `ChannelForUser` besides the
        // three writes (`/view`, `/notify_props`, `/roles`), which are POST/PUT and fall to
        // `partially_migrated`'s method fallback.
        //
        // The `Path` tuple is `(user_id, channel_id)`, matching the segment order — but the
        // *handler* validates the user first, where `get_channel_unread` validates the channel
        // first. Both orders are Go's; see `posts::get_posts_for_channel_around_last_unread`.
        .route(
            "/api/v4/users/{user_id}/channels/{channel_id}/posts/unread",
            partially_migrated_with_ids(
                &state,
                get(posts::get_posts_for_channel_around_last_unread),
            ),
        )
        // `BaseRoutes.ChannelsForTeam` (api.go:212) — the browse-channels list and its two
        // siblings. Unlike `/teams/name/{team_name}` above there is **no precedence puzzle
        // here**: every route gorilla registers under `/teams/{team_id}/channels/` is a static
        // literal, so neither router has a parameter to prefer over one. The literals we do not
        // serve (`/recommended`, `/ids`, `/search`, `/autocomplete`, `/search_autocomplete`,
        // `/managed_categories`) are simply unregistered and fall to `Router::fallback` whole —
        // asserted over HTTP in `tests/parity_team_channel_lists.rs`, because "still forwarded"
        // is a claim about the router, not about a handler.
        //
        // `/channels/name/{channel_name}` (registered above) is one segment deeper and cannot
        // collide with the two literals below.
        .route(
            "/api/v4/teams/{team_id}/channels",
            partially_migrated_with_ids(&state, get(channels::get_public_channels_for_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/private",
            partially_migrated_with_ids(&state, get(channels::get_private_channels_for_team)),
        )
        // `BaseRoutes.ChannelsForTeam.Handle("/autocomplete")` (api4/channel.go:68) — a literal
        // sibling of `/deleted` and `/private`, one segment deeper than `/teams/{team_id}`.
        // `BaseRoutes.ChannelsForTeam.Handle("/search")` (api4/channel.go:67) — the browse
        // dialog.
        .route(
            "/api/v4/teams/{team_id}/channels/search",
            partially_migrated_with_ids(&state, post(channels::search_channels_for_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/autocomplete",
            partially_migrated_with_ids(&state, get(channels::autocomplete_channels_for_team)),
        )
        // `BaseRoutes.ChannelsForTeam.Handle("/search_autocomplete")` (api4/channel.go:69) — the
        // literal beside `/autocomplete`, and a different handler with a different query and no
        // permission gate at all.
        .route(
            "/api/v4/teams/{team_id}/channels/search_autocomplete",
            partially_migrated_with_ids(
                &state,
                get(channels::autocomplete_channels_for_team_for_search),
            ),
        )
        // `BaseRoutes.ChannelsForTeam.Handle("/ids")` (api4/channel.go:66), POST-only — a third
        // static literal beside `/private` and `/deleted`, so the "no precedence puzzle here"
        // note above covers it unchanged. It was in that note's list of unregistered literals
        // until this route existed; `tests/parity/team_channel_lists.rs` moved it from the
        // forwarded set to the served one rather than dropping the assertion.
        .route(
            "/api/v4/teams/{team_id}/channels/ids",
            partially_migrated_with_ids(
                &state,
                post(channels::get_public_channels_by_ids_for_team),
            ),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/deleted",
            partially_migrated_with_ids(&state, get(channels::get_deleted_channels_for_team)),
        )
        // `BaseRoutes.Users.Handle("", ...)` (api4/user.go:34) — the bare `/users` collection.
        // It is one segment shorter than every `/api/v4/users/...` route registered above, so
        // axum sees a distinct path and there is no literal-versus-parameterised question to
        // answer: nothing that matched `{user_id}` or the `me`/`ids`/`username` literals can
        // match here, and nothing here could have matched them. Only `GET` is migrated; `POST`
        // (createUser) and the rest fall to `partially_migrated`'s method fallback.
        .route("/api/v4/users", partially_migrated(get(users::get_users)))
        // `BaseRoutes.Users.Handle("/known")` and `("/stats")` (api4/user.go:34, :37) — two more
        // literals beside `{user_id}`, so the same reasoning as `ids` and `autocomplete` above:
        // axum prefers a registered literal, and the `{user_id}` handler's exact-26-character
        // rule was forwarding both of these anyway. `/users/stats/filtered` is one segment
        // deeper, unregistered, and falls to `Router::fallback` whole.
        .route(
            "/api/v4/users/known",
            partially_migrated(get(users::get_known_users)),
        )
        .route(
            "/api/v4/users/stats",
            partially_migrated(get(users::get_total_users_stats)),
        )
        // `/users/stats/filtered` is one segment deeper again — a different handler with its own
        // query parameters, and forwarded until `getFilteredUsersStats` landed.
        .route(
            "/api/v4/users/stats/filtered",
            partially_migrated(get(users::get_filtered_users_stats)),
        )
        // `BaseRoutes.User.Handle("/terms_of_service")` (api4/user.go:61) — one segment deeper
        // than `{user_id}`. Registered GET only: the `POST` on the same path
        // (`saveUserTermsOfService`, :60) falls to `partially_migrated`'s method fallback.
        //
        // The handler ignores `{user_id}` entirely and answers for the session's user; the id
        // middleware still applies, because gorilla's `[A-Za-z0-9]+` still has to match for the
        // request to reach a handler at all.
        .route(
            "/api/v4/users/{user_id}/terms_of_service",
            partially_migrated_with_ids(&state, get(users::get_user_terms_of_service)),
        )
        // `BaseRoutes.Teams.Handle("", ...)` (api4/team.go:35) — the bare `/teams` collection,
        // and the same non-question as `/api/v4/users` above: it is one segment shorter than
        // every `/api/v4/teams/...` route registered earlier, so axum sees a distinct path and
        // there is no literal-versus-parameter precedence to settle. Nothing that matched
        // `{team_id}` or the `name` literal can match here, and nothing here could have matched
        // them.
        .route(
            "/api/v4/teams",
            partially_migrated(get(teams::get_all_teams).post(teams::create_team)),
        )
        // `BaseRoutes.Teams.Handle("/search")` (api4/team.go:37). A static sibling of
        // `{team_id}`; gorilla registered `/search` *after* `{team_id:[A-Za-z0-9]+}`, but
        // `search` is id-shaped and would have matched it — so on Go the literal wins only
        // because `{team_id}` carries no POST handler, and on axum the literal wins outright.
        // Same answer either way.
        .route(
            "/api/v4/teams/search",
            partially_migrated(post(teams::search_teams)),
        )
        // `BaseRoutes.Teams.Handle("/invites/email")` (api4/team.go:74). Two static segments
        // under `/teams/`, so no overlap with `{team_id}` — `invites` would have matched the
        // id class, but the path is one segment longer than `{team_id}` and matches nothing
        // else gorilla registers.
        .route(
            "/api/v4/teams/invites/email",
            partially_migrated(axum::routing::delete(teams::invalidate_all_email_invites)),
        )
        // `BaseRoutes.Roles` (api4/api.go). The literal `names` sits beside `{role_id}` and
        // gorilla registered `{role_id:[A-Za-z0-9]+}` *first*, so `GET /roles/names` is a
        // `getRole` call there with `role_id = "names"` and 400s. Registered POST-only here, so
        // a GET falls to `partially_migrated`'s method fallback and Go answers exactly that.
        .route(
            "/api/v4/roles/names",
            partially_migrated(post(roles::get_roles_by_names)),
        )
        // `role_name` is deliberately not id-shaped: Go's class is `[a-z0-9_]+`, narrower than
        // the `[A-Za-z0-9]+` the id middleware enforces, so the handler carries its own mux
        // forward like `username` and `category` do. One segment deeper than `{role_id}`, so
        // there is no conflict with it.
        .route(
            "/api/v4/roles/name/{role_name}",
            partially_migrated(get(roles::get_role_by_name)),
        )
        // `BaseRoutes.Roles.Handle("", ...)` (api4/role.go:24) — the bare `/roles` collection,
        // one segment shorter than every `/api/v4/roles/...` route below, so axum sees a
        // distinct path and there is no literal-versus-parameter precedence to settle. `GET`
        // only; nothing else is registered on it in Go either.
        .route(
            "/api/v4/users/{user_id}/audits",
            partially_migrated_with_ids(&state, get(audits::get_user_audits)),
        )
        .route(
            "/api/v4/teams/{team_id}/channels/recommended",
            partially_migrated_with_ids(&state, get(channels::get_recommended_channels_for_team)),
        )
        .route(
            "/api/v4/channels/{channel_id}/common_teams",
            partially_migrated_with_ids(&state, get(common_teams::get_common_teams)),
        )
        .route(
            "/api/v4/hooks/incoming",
            partially_migrated(
                get(webhooks::get_incoming_hooks).post(webhooks::create_incoming_hook),
            ),
        )
        .route(
            "/api/v4/hooks/outgoing",
            partially_migrated(
                get(webhooks::get_outgoing_hooks).post(webhooks::create_outgoing_hook),
            ),
        )
        .route(
            "/api/v4/hooks/incoming/{hook_id}",
            partially_migrated_with_ids(
                &state,
                get(webhooks::get_incoming_hook)
                    .put(webhooks::update_incoming_hook)
                    .delete(webhooks::delete_incoming_hook),
            ),
        )
        .route(
            "/api/v4/hooks/outgoing/{hook_id}",
            partially_migrated_with_ids(
                &state,
                get(webhooks::get_outgoing_hook)
                    .put(webhooks::update_outgoing_hook)
                    .delete(webhooks::delete_outgoing_hook),
            ),
        )
        // `BaseRoutes.OutgoingHooks.Handle("/{hook_id:[A-Za-z0-9]+}/regen_token")`
        // (api4/webhook.go:22) — a segment deeper than `{hook_id}`, so no precedence question.
        .route(
            "/api/v4/hooks/outgoing/{hook_id}/regen_token",
            partially_migrated_with_ids(&state, post(webhooks::regen_outgoing_hook_token)),
        )
        .route(
            "/api/v4/users/{user_id}/oauth/apps/authorized",
            partially_migrated_with_ids(&state, get(oauth::get_authorized_oauth_apps)),
        )
        .route(
            "/api/v4/channels/{channel_id}/moderations",
            partially_migrated_with_ids(&state, get(channels::get_channel_moderations)),
        )
        .route(
            "/api/v4/channels/{channel_id}/bookmarks",
            // The `GET` is not licence-gated and lives in `channels`; the `POST` is, and lives in
            // `licensed_features`. Both are declared here, where the path is, rather than the
            // path being registered twice.
            partially_migrated_with_ids(
                &state,
                get(channels::list_channel_bookmarks)
                    .post(licensed_features::create_channel_bookmark),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/member_counts_by_group",
            partially_migrated_with_ids(&state, get(channels::get_channel_member_counts_by_group)),
        )
        .route(
            "/api/v4/groups",
            partially_migrated(get(groups::get_groups).post(groups::create_group)),
        )
        .route(
            "/api/v4/users/{user_id}/groups",
            partially_migrated_with_ids(&state, get(groups::get_groups_by_user_id)),
        )
        .route(
            "/api/v4/limits/server",
            partially_migrated(get(limits::get_server_limits)),
        )
        .route(
            "/api/v4/terms_of_service",
            partially_migrated(get(terms_of_service::get_latest_terms_of_service)),
        )
        .route(
            "/api/v4/oauth/apps",
            partially_migrated(get(oauth::get_oauth_apps).post(oauth::create_oauth_app)),
        )
        // `BaseRoutes.OAuthApps.Handle("/register")` (api4/oauth.go:24) — a **literal** sibling of
        // `{app_id}` below, so axum's literal-first precedence puts it here. Go registers it as a
        // distinct route too, and with `APIHandler` rather than `APISessionRequired`: it is the
        // one route in this file that takes no session.
        .route(
            "/api/v4/oauth/apps/register",
            partially_migrated(post(oauth::register_oauth_client)),
        )
        .route(
            "/api/v4/oauth/apps/{app_id}",
            partially_migrated_with_ids(
                &state,
                get(oauth::get_oauth_app)
                    .put(oauth::update_oauth_app)
                    .delete(oauth::delete_oauth_app),
            ),
        )
        .route(
            "/api/v4/oauth/apps/{app_id}/regen_secret",
            partially_migrated_with_ids(&state, post(oauth::regenerate_oauth_app_secret)),
        )
        .route(
            "/api/v4/oauth/apps/{app_id}/info",
            partially_migrated_with_ids(&state, get(oauth::get_oauth_app_info)),
        )
        .route(
            "/api/v4/license/client",
            partially_migrated(get(license::get_client_license)),
        )
        .route(
            "/api/v4/roles",
            partially_migrated(get(roles::get_all_roles)),
        )
        // `[A-Za-z0-9]+` matches the id middleware's rule exactly. `.../{role_id}/patch` (one
        // segment longer) falls to `Router::fallback` and stays forwarded.
        .route(
            "/api/v4/roles/{role_id}",
            partially_migrated_with_ids(&state, get(roles::get_role)),
        )
        // `BaseRoutes.Post` (api4/api.go:239) — `/posts/{post_id:[A-Za-z0-9]+}`, which Go gives a
        // GET, a PUT and a DELETE. All three are served here now. Every other `/posts/...` path Go
        // registers is either one segment deeper (`/patch`, `/thread`, `/files/info`, …) or a
        // literal sibling of `{post_id}` (`/posts/ids`, `/posts/ephemeral`); the ones not
        // registered below fall to `Router::fallback` and stay forwarded.
        .route(
            "/api/v4/posts/{post_id}",
            partially_migrated_with_ids(
                &state,
                get(posts::get_post)
                    .put(post_writes::update_post)
                    .delete(post_writes::delete_post),
            ),
        )
        // `BaseRoutes.Post.Handle("/patch")` (api4/post.go:44) — one segment deeper than the route
        // above, PUT-only in Go.
        .route(
            "/api/v4/posts/{post_id}/patch",
            partially_migrated_with_ids(&state, axum::routing::put(post_writes::patch_post)),
        )
        // `BaseRoutes.Posts.Handle("/ids")` (api4/post.go:28). The literal `ids` sits where
        // `{post_id}` sits above; axum prefers the literal. Nothing that used to be answered
        // stops being answered — the route above enforces exact-26-character ids and `ids` is
        // three characters, so this path was forwarded, not served. POST-only, which is all Go
        // registers here; `/posts/ephemeral` is a sibling this router still leaves to Go.
        .route(
            "/api/v4/posts/ids",
            partially_migrated(post(posts::get_posts_by_ids)),
        )
        // `BaseRoutes.Posts.Handle("")` (api4/post.go:25) — the route the product is built
        // around. One segment shorter than `{post_id}`, so it collides with nothing; the GET,
        // PUT and DELETE Go does not register on this path keep going to Go through
        // `partially_migrated`.
        .route(
            "/api/v4/posts",
            partially_migrated(post(post_writes::create_post)),
        )
        // `BaseRoutes.Posts.Handle("/ephemeral")` (api4/post.go:29) — the second literal sibling
        // of `{post_id}`, and the one that has to carry four methods rather than one.
        //
        // **A literal un-serves its parameterised sibling for every method, and axum does not
        // backtrack.** `GET /posts/ephemeral` was answered here before this line existed: it
        // reached `posts::get_post`, whose `RequirePostId` refused the nine-character segment
        // with a **400 `api.context.invalid_url_param.app_error`**. Registering the literal with
        // POST alone would have sent that GET — and the PUT and the DELETE — to Go instead,
        // silently. So the other three methods are registered too, answering exactly what
        // `{post_id}` answered: the same 400, for the same reason, because `ephemeral` is not a
        // 26-character id. `tests/parity/post_creates.rs` re-asks all four and would fail if any
        // of them started being forwarded.
        .route(
            "/api/v4/posts/ephemeral",
            partially_migrated(
                post(post_writes::create_ephemeral_post)
                    .get(invalid_post_id_param)
                    .put(invalid_post_id_param)
                    .delete(invalid_post_id_param),
            ),
        )
        // `BaseRoutes.Post.Handle("/thread")` (api4/post.go:31) — one segment deeper than the
        // route above, so neither shadows the other. Its literal siblings under `{post_id}`
        // (`/edit_history`, `/info`, `/files/info`, `/reveal`, `/patch`, `/pin`, …) are not
        // registered here at all and fall to `Router::fallback` whole; `partially_migrated`
        // keeps every non-GET method on this exact path going to Go.
        .route(
            "/api/v4/posts/{post_id}/thread",
            partially_migrated_with_ids(&state, get(posts::get_post_thread)),
        )
        // `BaseRoutes.Post.Handle("/reactions")` (api4/reaction.go:16) — a sibling of
        // `/thread` above and one segment deeper than `/posts/{post_id}`, so nothing here
        // shadows anything. Registered GET only: `POST /api/v4/reactions` is a different path
        // entirely (`saveReaction` hangs off `BaseRoutes.Reactions`, not off the post), and
        // `DELETE .../reactions/{emoji_name}` is one segment deeper still — both fall to
        // `Router::fallback` and stay forwarded.
        .route(
            "/api/v4/posts/{post_id}/reactions",
            partially_migrated_with_ids(&state, get(reactions::get_reactions)),
        )
        // `BaseRoutes.Posts.Handle("/ids/reactions")` (api4/reaction.go:19). The literal `ids`
        // sits where `{post_id}` sits above and axum prefers the literal, so this wins for the
        // one path that spells it. Nothing that used to be answered stops being answered: the
        // route above is wrapped in the exact-26-char id middleware and `ids` is three
        // characters, so `POST /posts/ids/reactions` was being forwarded to Go, not served.
        // Registered POST-only — Go has no other method on this path.
        .route(
            "/api/v4/posts/ids/reactions",
            partially_migrated(post(reactions::get_bulk_reactions)),
        )
        // `BaseRoutes.Reactions.Handle("")` (api4/reaction.go:15) — POST only. There is no GET
        // on this path in Go at all, so `partially_migrated` forwards one and Go 404s it, which
        // is what it did before.
        // `BaseRoutes.Drafts.Handle("")` (api4/drafts.go:15) — POST only.
        .route(
            "/api/v4/drafts",
            partially_migrated(post(drafts::upsert_draft)),
        )
        // The two `deleteDraft` registrations (api4/drafts.go:19-20). They are one handler; the
        // shallower path leaves `thread_id` empty, which is the root id a channel-level draft
        // has.
        .route(
            "/api/v4/users/{user_id}/channels/{channel_id}/drafts",
            partially_migrated(axum::routing::delete(drafts::delete_draft)),
        )
        .route(
            "/api/v4/users/{user_id}/channels/{channel_id}/drafts/{thread_id}",
            partially_migrated(axum::routing::delete(drafts::delete_draft)),
        )
        .route(
            "/api/v4/reactions",
            partially_migrated(post(reactions::save_reaction)),
        )
        // `BaseRoutes.ReactionByNameForPostForUser.Handle("")` (api4/reaction.go:17) — the
        // deepest path under `/users`, four segments below `{user_id}`. Nothing else claims it,
        // so no precedence question arises with the `{user_id}` reads above.
        .route(
            "/api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}",
            partially_migrated(axum::routing::delete(reactions::delete_reaction)),
        )
        // `BaseRoutes.Post.Handle("/edit_history")` (api4/post.go:30) — another sibling of
        // `/thread` and `/reactions`, one segment deeper than `/posts/{post_id}`.
        .route(
            "/api/v4/posts/{post_id}/edit_history",
            partially_migrated_with_ids(&state, get(posts::get_edit_history_for_post)),
        )
        // `BaseRoutes.Post.Handle("/pin")` and `.Handle("/unpin")` (api4/post.go:47-48) — two more
        // siblings of `/thread` under `{post_id}`, POST-only in Go. They are one handler there
        // (`saveIsPinnedPost`) reached through two registrations, which is why they are two
        // `.route` calls here and not one path with two methods.
        .route(
            "/api/v4/posts/{post_id}/pin",
            partially_migrated_with_ids(&state, post(post_writes::pin_post)),
        )
        .route(
            "/api/v4/posts/{post_id}/unpin",
            partially_migrated_with_ids(&state, post(post_writes::unpin_post)),
        )
        // `BaseRoutes.Post.Handle("/files/info")` (api4/post.go:33) — two segments deeper than
        // `/posts/{post_id}`, so it shadows nothing and nothing shadows it. `POST /files` and
        // `GET /files/{file_id}/info` are a different subtree entirely (`BaseRoutes.Files`,
        // api.go:244); this route hangs off the *post*.
        .route(
            "/api/v4/posts/{post_id}/files/info",
            partially_migrated_with_ids(&state, get(posts::get_file_infos_for_post)),
        )
        // `BaseRoutes.File.Handle("/info")` (api4/file.go:39). `{file_id}` is id-shaped —
        // gorilla's class is `[A-Za-z0-9]+` (api.go:245) — so the id-charset middleware applies.
        .route(
            "/api/v4/files/{file_id}/info",
            partially_migrated_with_ids(&state, get(files::get_file_info)),
        )
        // `BaseRoutes.File.Handle("")` (api4/file.go:36) and its two derived-image siblings
        // (:37, :38). Go registers each with `Methods(GET, HEAD)`, and **axum's `get` answers
        // HEAD too**, dispatching it to the same handler with the body removed — so these three
        // registrations cover six route+method pairs. `serve_content` still branches on the
        // method itself, because Go's `ServeContent` sets `Content-Length` on a HEAD and only
        // skips the copy.
        //
        // `POST /files` (uploadFileStream) is on the same path as the first of them and falls to
        // `partially_migrated`'s method fallback.
        .route(
            "/api/v4/files/{file_id}",
            partially_migrated_with_ids(&state, get(files::get_file)),
        )
        .route(
            "/api/v4/files/{file_id}/thumbnail",
            partially_migrated_with_ids(&state, get(files::get_file_thumbnail)),
        )
        .route(
            "/api/v4/files/{file_id}/preview",
            partially_migrated_with_ids(&state, get(files::get_file_preview)),
        )
        // `BaseRoutes.Channel.Handle("/pinned")` (api4/channel.go:60) — one segment deeper than
        // `/channels/{channel_id}` and a sibling of `/stats`, `/members` and `/posts`, all of
        // which are static at that position. Nothing to prefer, in either router.
        .route(
            "/api/v4/channels/{channel_id}/pinned",
            partially_migrated_with_ids(&state, get(channels::get_pinned_posts)),
        )
        // `BaseRoutes.Channel.Handle("/timezones")` (api4/channel.go:94) — a sibling of
        // `/pinned`, `/stats` and `/members`, all static at that position. Note the *system*
        // route `/api/v4/system/timezones` (api4/system.go:44) is a different subtree entirely
        // and stays forwarded.
        .route(
            "/api/v4/channels/{channel_id}/timezones",
            partially_migrated_with_ids(&state, get(channels::get_channel_members_timezones)),
        )
        // `BaseRoutes.Emoji` (api.go:286). The precedence question here is the *reverse* of
        // axum's instinct and it matters: gorilla adds the `PathPrefix("/emoji")` subrouter
        // (api.go:285) **before** the `PathPrefix("/emoji/{emoji_id}")` one, so `/emoji/
        // autocomplete` is answered by that subrouter's own literal and never reaches
        // `getEmoji`. axum prefers a literal too — but only one that is registered, and this
        // router does not register it. The handler forwards the literals that ordering owns;
        // see `emoji::EMOJI_SHADOWED_LITERALS` for why the list has exactly one entry.
        //
        // `/emoji/{emoji_id}/image` is one segment deeper, so it shadows nothing; registered
        // below with the other three stored-image routes.
        // `BaseRoutes.Emojis.Handle("")` (api4/emoji.go:15) — the bare `/emoji` collection, one
        // segment shorter than `{emoji_id}` below, so axum sees a distinct path and there is no
        // precedence question of the kind that route's comment describes. `GET` only; `POST`
        // (createEmoji) falls to `partially_migrated`'s method fallback.
        .route(
            "/api/v4/emoji",
            partially_migrated(get(emoji::get_emoji_list)),
        )
        .route(
            "/api/v4/emoji/{emoji_id}",
            partially_migrated_with_ids(&state, get(emoji::get_emoji)),
        )
        // `BaseRoutes.EmojiByName` (api.go:287). One segment deeper than `{emoji_id}` above, so
        // neither shadows the other, and `emoji_name` is **not** id-shaped — Go's class is
        // `[A-Za-z0-9\_\-\+]+` — so the id-charset middleware must not apply. The handler
        // carries its own mux forward, like `username`, `role_name` and `channel_name`.
        .route(
            "/api/v4/emoji/name/{emoji_name}",
            partially_migrated(get(emoji::get_emoji_by_name)),
        )
        // `BaseRoutes.Emojis.Handle("/autocomplete")` (api4/emoji.go:22) — a literal sibling of
        // `{emoji_id}`, which axum prefers. Registered GET-only; `/emoji/names` and
        // `/emoji/search` are POST in Go and stay forwarded.
        // `BaseRoutes.Emojis.Handle("/names")` (api4/emoji.go:24) — posted once per channel
        // load with the emoji names found in the page.
        // `BaseRoutes.Emojis.Handle("/search")` (api4/emoji.go:25) — the emoji picker posts this
        // on every keystroke. `GET` is gorilla's fallthrough to `{emoji_id}`, as for `/names`.
        .route(
            "/api/v4/emoji/search",
            partially_migrated(post(emoji::search_emojis).get(emoji::get_emoji_search_literal)),
        )
        .route(
            "/api/v4/emoji/names",
            partially_migrated(post(emoji::get_emojis_by_names).get(emoji::get_emoji_name_literal)),
        )
        .route(
            "/api/v4/emoji/autocomplete",
            partially_migrated(get(emoji::autocomplete_emojis)),
        )
        // The four stored-image routes. Each is one segment deeper than a route already here, so
        // none of them shadows anything, and each carries its own permission rule — see
        // `images` for the table.
        .route(
            "/api/v4/emoji/{emoji_id}/image",
            partially_migrated_with_ids(&state, get(images::get_emoji_image)),
        )
        .route(
            "/api/v4/users/{user_id}/image",
            partially_migrated_with_ids(&state, get(images::get_profile_image)),
        )
        .route(
            "/api/v4/teams/{team_id}/image",
            partially_migrated_with_ids(&state, get(images::get_team_icon)),
        )
        // `BaseRoutes.Brand.Handle("/image")` (api4/brand.go:14). The GET is
        // `APIHandlerTrustRequester` — **unauthenticated** — and the DELETE is session-required
        // with `edit_brand`; the POST between them uploads a multipart image and stays
        // forwarded through `partially_migrated`'s method fallback.
        .route(
            "/api/v4/brand/image",
            partially_migrated(get(images::get_brand_image).delete(images::delete_brand_image)),
        )
        // `BaseRoutes.Exports` / `BaseRoutes.Export` (api.go:302, :304) and the two import
        // routes. `{export_name}` and `{import_name}` are **not** id-shaped — gorilla's pattern
        // is `.+\.zip`, which allows dots, dashes and slashes — so the id-charset middleware
        // must not apply; the handlers carry their own suffix check instead and forward a name
        // gorilla would not have routed. A multi-segment name does not match axum's single
        // segment at all and falls to `Router::fallback`, which is the same answer.
        .route(
            "/api/v4/exports",
            partially_migrated(get(exports::list_exports)),
        )
        .route(
            "/api/v4/exports/{export_name}",
            partially_migrated(get(exports::download_export).delete(exports::delete_export)),
        )
        .route(
            "/api/v4/exports/{export_name}/presign-url",
            partially_migrated(post(exports::generate_presign_url_export)),
        )
        .route(
            "/api/v4/imports",
            partially_migrated(get(exports::list_imports)),
        )
        .route(
            "/api/v4/imports/{import_name}",
            partially_migrated(axum::routing::delete(exports::delete_import)),
        )
        // `BaseRoutes.Upload` (api.go:249) and `BaseRoutes.User.Handle("/uploads")`
        // (api4/user.go:118). Both are reads of `UploadSessions` rows and neither touches the
        // file backend. The `POST` on each path — `uploadData` and nothing, respectively — falls
        // to `partially_migrated`'s method fallback.
        .route(
            "/api/v4/uploads/{upload_id}",
            partially_migrated_with_ids(&state, get(uploads::get_upload)),
        )
        .route(
            "/api/v4/users/{user_id}/uploads",
            partially_migrated_with_ids(&state, get(uploads::get_uploads_for_user)),
        )
        // `BaseRoutes.ChannelCategories` (api.go:231) — **all eight**, three GETs and five
        // writes, on three paths. `partially_migrated_with_ids` still wraps each one: the
        // fallback now catches only methods gorilla never registered here (a `POST` to
        // `/order`, say), and the id-charset layer is what forwards a malformed `user_id` or
        // `team_id` for Go's own mux 404.
        //
        // One segment deeper than `/users/{user_id}/teams/{team_id}/channels` above and a
        // sibling of `…/channels/members`; all three are static at that position, so there is
        // nothing for either router to prefer.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/channels/categories",
            partially_migrated_with_ids(
                &state,
                get(sidebar::get_categories_for_team_for_user)
                    .post(sidebar::create_category_for_team_for_user)
                    .put(sidebar::update_categories_for_team_for_user),
            ),
        )
        // The literal `order` beside `{category}` below. gorilla registers it first
        // (api4/channel.go:80 against :82) and axum prefers a static segment outright, so both
        // routers serve `getCategoryOrderForTeamForUser` here — same answer, different reason.
        // The `PUT` sits on the same route for the same reason.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/channels/categories/order",
            partially_migrated_with_ids(
                &state,
                get(sidebar::get_category_order_for_team_for_user)
                    .put(sidebar::update_category_order_for_team_for_user),
            ),
        )
        // Deliberately `{category}` and not `{category_id}`: Go's mux class here is
        // `[A-Za-z0-9_-]+`, and a default category's id is `{type}_{userId}_{teamId}`. Naming it
        // `*_id` would enrol it in `parameter_is_id_shaped`'s `[A-Za-z0-9]+` rule and forward
        // every underscore-bearing id — which is to say the common case. The handler carries
        // Go's own charset instead, like `username`, `role_name` and `channel_name` do.
        .route(
            "/api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category}",
            partially_migrated_with_ids(
                &state,
                get(sidebar::get_category_for_team_for_user)
                    .put(sidebar::update_category_for_team_for_user)
                    .delete(sidebar::delete_category_for_team_for_user),
            ),
        )
        // ---- compliance, IP filtering, the AI-bridge test helper and scheduled posts ----
        //
        // Fourteen routes across four small families with four different refusals between them —
        // two at 501, two at 400, and one family whose gate has two arms sharing a status. See
        // `compliance.rs` and `feature_gates.rs`.
        .route(
            "/api/v4/compliance/reports",
            partially_migrated(
                get(compliance::get_compliance_reports).post(compliance::create_compliance_report),
            ),
        )
        .route(
            "/api/v4/compliance/reports/{report_id}",
            partially_migrated_with_ids(&state, get(compliance::get_compliance_report)),
        )
        .route(
            "/api/v4/compliance/reports/{report_id}/download",
            partially_migrated_with_ids(&state, get(compliance::download_compliance_report)),
        )
        .route(
            "/api/v4/ip_filtering",
            partially_migrated(
                get(feature_gates::get_ip_filters).post(feature_gates::apply_ip_filters),
            ),
        )
        .route(
            "/api/v4/ip_filtering/my_ip",
            partially_migrated(get(feature_gates::my_ip)),
        )
        // Three methods on one path, all gated on `ServiceSettings.EnableTesting`.
        .route(
            "/api/v4/system/e2e/ai_bridge",
            partially_migrated(
                get(feature_gates::get_ai_bridge_test_helper)
                    .put(feature_gates::put_ai_bridge_test_helper)
                    .delete(feature_gates::delete_ai_bridge_test_helper),
            ),
        )
        // `/posts/schedule` and `/posts/scheduled` are two different literals under `/posts`, and
        // both are literal siblings of `{post_id}` — axum prefers the literal and gorilla
        // registers them first, so the routers agree.
        .route(
            "/api/v4/posts/schedule",
            partially_migrated(post(feature_gates::create_schedule_post)),
        )
        .route(
            "/api/v4/posts/schedule/{scheduled_post_id}",
            partially_migrated_with_ids(
                &state,
                put(feature_gates::update_scheduled_post)
                    .delete(feature_gates::delete_scheduled_post),
            ),
        )
        .route(
            "/api/v4/posts/scheduled/team/{team_id}",
            partially_migrated_with_ids(&state, get(feature_gates::get_team_scheduled_posts)),
        )
        // ---- cloud and connected workspaces (2026-09-07) ----
        //
        // Twelve cloud routes refusing with a **400** and thirteen connected-workspace routes
        // refusing with a 501 — three neighbouring families, three shapes. See `cloud.rs` and
        // `connected_workspaces.rs`.
        //
        // `/cloud/preview/modal_data` and `/cloud/webhook` are deliberately absent: the first has
        // no cloud gate and the second uses a different authentication wrapper.
        .route(
            "/api/v4/cloud/products",
            partially_migrated(get(cloud::get_cloud_products)),
        )
        .route(
            "/api/v4/cloud/limits",
            partially_migrated(get(cloud::get_cloud_limits)),
        )
        .route(
            "/api/v4/cloud/installation",
            partially_migrated(get(cloud::get_installation)),
        )
        .route(
            "/api/v4/cloud/check-cws-connection",
            partially_migrated(get(cloud::handle_check_cws_connection)),
        )
        .route(
            "/api/v4/cloud/customer",
            partially_migrated(get(cloud::get_cloud_customer).put(cloud::update_cloud_customer)),
        )
        .route(
            "/api/v4/cloud/customer/address",
            partially_migrated(put(cloud::update_cloud_customer_address)),
        )
        .route(
            "/api/v4/cloud/subscription",
            partially_migrated(get(cloud::get_subscription)),
        )
        .route(
            "/api/v4/cloud/subscription/invoices",
            partially_migrated(get(cloud::get_invoices_for_subscription)),
        )
        // `{invoice_id}` is `[_A-Za-z0-9]+` in Go — it allows an underscore, so it is **not**
        // id-shaped and must not get the id-charset middleware.
        .route(
            "/api/v4/cloud/subscription/invoices/{invoice_id}/pdf",
            partially_migrated(get(cloud::get_subscription_invoice_pdf)),
        )
        .route(
            "/api/v4/cloud/validate-business-email",
            partially_migrated(post(cloud::validate_business_email)),
        )
        .route(
            "/api/v4/cloud/validate-workspace-business-email",
            partially_migrated(post(cloud::validate_workspace_business_email)),
        )
        .route(
            "/api/v4/remotecluster",
            partially_migrated(
                get(connected_workspaces::get_remote_clusters)
                    .post(connected_workspaces::create_remote_cluster),
            ),
        )
        .route(
            "/api/v4/remotecluster/accept_invite",
            partially_migrated(post(connected_workspaces::remote_cluster_accept_invite)),
        )
        // `accept_invite`, `confirm_invite`, `msg`, `ping` and `upload` are literal siblings of
        // `{remote_id}` — all contain `_` or are shorter than an id, so Go's
        // `[A-Za-z0-9]+` class routes them to their own handlers and axum prefers the literal.
        // Only `accept_invite` is registered here; the rest use a different auth wrapper.
        .route(
            "/api/v4/remotecluster/{remote_id}",
            partially_migrated_with_ids(
                &state,
                get(connected_workspaces::get_remote_cluster)
                    .patch(connected_workspaces::patch_remote_cluster)
                    .delete(connected_workspaces::delete_remote_cluster),
            ),
        )
        .route(
            "/api/v4/remotecluster/{remote_id}/generate_invite",
            partially_migrated_with_ids(
                &state,
                post(connected_workspaces::generate_remote_cluster_invite),
            ),
        )
        .route(
            "/api/v4/remotecluster/{remote_id}/sharedchannelremotes",
            partially_migrated_with_ids(
                &state,
                get(connected_workspaces::get_shared_channel_remotes_by_remote_cluster),
            ),
        )
        .route(
            "/api/v4/remotecluster/{remote_id}/channels/{channel_id}/invite",
            partially_migrated_with_ids(
                &state,
                post(connected_workspaces::invite_remote_cluster_to_channel),
            ),
        )
        .route(
            "/api/v4/remotecluster/{remote_id}/channels/{channel_id}/uninvite",
            partially_migrated_with_ids(
                &state,
                post(connected_workspaces::uninvite_remote_cluster_to_channel),
            ),
        )
        // `remote_info` contains an underscore, so Go's `{team_id:[A-Za-z0-9]+}` never matches it
        // and the literal is the only route either router can pick.
        .route(
            "/api/v4/sharedchannels/remote_info/{remote_id}",
            partially_migrated_with_ids(&state, get(connected_workspaces::get_remote_cluster_info)),
        )
        .route(
            "/api/v4/sharedchannels/{team_id}",
            partially_migrated_with_ids(&state, get(connected_workspaces::get_shared_channels)),
        )
        .route(
            "/api/v4/sharedchannels/{channel_id}/remotes",
            partially_migrated_with_ids(
                &state,
                get(connected_workspaces::get_shared_channel_remotes),
            ),
        )
        // ---- recaps (2026-09-07) ----
        //
        // Fifteen routes behind one **configuration** gate, not a licence — see `recaps.rs`. An
        // operator can enable them, so every handler reads the live config and forwards when they
        // have.
        .route(
            "/api/v4/recaps",
            partially_migrated(get(recaps::get_recaps).post(recaps::create_recap)),
        )
        .route(
            "/api/v4/recaps/limit_status",
            partially_migrated(get(recaps::get_recap_limit_status)),
        )
        .route(
            "/api/v4/recaps/mark_viewed",
            partially_migrated(post(recaps::mark_recaps_as_viewed)),
        )
        // `limit_status` and `mark_viewed` are literal siblings of `{recap_id}`; axum prefers a
        // literal, and gorilla registers them first, so both routers agree.
        .route(
            "/api/v4/recaps/{recap_id}",
            partially_migrated_with_ids(
                &state,
                get(recaps::get_recap).delete(recaps::delete_recap),
            ),
        )
        .route(
            "/api/v4/recaps/{recap_id}/read",
            partially_migrated_with_ids(&state, post(recaps::mark_recap_as_read)),
        )
        .route(
            "/api/v4/recaps/{recap_id}/regenerate",
            partially_migrated_with_ids(&state, post(recaps::regenerate_recap)),
        )
        .route(
            "/api/v4/scheduled_recaps",
            partially_migrated(
                get(recaps::get_scheduled_recaps).post(recaps::create_scheduled_recap),
            ),
        )
        .route(
            "/api/v4/scheduled_recaps/{scheduled_recap_id}",
            partially_migrated_with_ids(
                &state,
                get(recaps::get_scheduled_recap)
                    .put(recaps::update_scheduled_recap)
                    .delete(recaps::delete_scheduled_recap),
            ),
        )
        .route(
            "/api/v4/scheduled_recaps/{scheduled_recap_id}/pause",
            partially_migrated_with_ids(&state, post(recaps::pause_scheduled_recap)),
        )
        .route(
            "/api/v4/scheduled_recaps/{scheduled_recap_id}/resume",
            partially_migrated_with_ids(&state, post(recaps::resume_scheduled_recap)),
        )
        // ---- licence-gated families (2026-09-07) ----
        //
        // Seventeen routes whose first statement is a licence test — see `licensed_features.rs`.
        // The `{post_id}`, `{team_id}` and `{content_reviewer_id}` segments are id-shaped and get
        // the mux charset check; nothing downstream of the refusal reads them.
        .route(
            "/api/v4/content_flagging/flag/config",
            partially_migrated(get(licensed_features::get_flagging_configuration)),
        )
        .route(
            "/api/v4/content_flagging/fields",
            partially_migrated(get(licensed_features::get_content_flagging_fields)),
        )
        .route(
            "/api/v4/content_flagging/config",
            partially_migrated(
                get(licensed_features::get_content_flagging_settings)
                    .put(licensed_features::save_content_flagging_settings),
            ),
        )
        .route(
            "/api/v4/content_flagging/team/{team_id}/status",
            partially_migrated_with_ids(
                &state,
                get(licensed_features::get_team_post_flagging_feature_status),
            ),
        )
        .route(
            "/api/v4/content_flagging/team/{team_id}/reviewers/search",
            partially_migrated_with_ids(&state, get(licensed_features::search_reviewers)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}",
            partially_migrated_with_ids(&state, get(licensed_features::get_flagged_post)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/flag",
            partially_migrated_with_ids(&state, post(licensed_features::flag_post)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/field_values",
            partially_migrated_with_ids(&state, get(licensed_features::get_post_property_values)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/remove",
            partially_migrated_with_ids(&state, put(licensed_features::remove_flagged_post)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/keep",
            partially_migrated_with_ids(&state, put(licensed_features::keep_flagged_post)),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/report",
            partially_migrated_with_ids(
                &state,
                post(licensed_features::generate_flagged_post_report),
            ),
        )
        .route(
            "/api/v4/content_flagging/post/{post_id}/assign/{content_reviewer_id}",
            partially_migrated_with_ids(
                &state,
                post(licensed_features::assign_flagged_post_reviewer),
            ),
        )
        // The bookmark writes on the item path. The collection's `POST` is declared beside its
        // `GET` above, where the path already was.
        .route(
            "/api/v4/channels/{channel_id}/bookmarks/{bookmark_id}",
            partially_migrated_with_ids(
                &state,
                axum::routing::patch(licensed_features::update_channel_bookmark)
                    .delete(licensed_features::delete_channel_bookmark),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/bookmarks/{bookmark_id}/sort_order",
            partially_migrated_with_ids(
                &state,
                post(licensed_features::update_channel_bookmark_sort_order),
            ),
        )
        // ---- data retention (2026-09-07) ----
        //
        // Fifteen routes whose whole behaviour on an unlicensed server is a 501 with the checks
        // ahead of it — see `data_retention.rs`. `/policies` carries a GET and a POST;
        // `/policies/{policy_id}` a GET, a PATCH and a DELETE; the two `/teams` and `/channels`
        // children each carry three methods. The two `/search` children are **not** registered:
        // they are ordinary searches with no licence gate, and they belong with `/teams/search`.
        //
        // `{policy_id}` is id-shaped, so `partially_migrated_with_ids` forwards a segment Go's
        // mux would not have matched — but note that a segment which *does* match and is still
        // not a valid id reaches the handler and is deliberately **not** rejected there.
        .route(
            "/api/v4/data_retention/policy",
            partially_migrated(get(data_retention::get_global_policy)),
        )
        .route(
            "/api/v4/data_retention/policies_count",
            partially_migrated(get(data_retention::get_policies_count)),
        )
        .route(
            "/api/v4/data_retention/policies",
            partially_migrated(
                get(data_retention::get_policies).post(data_retention::create_policy),
            ),
        )
        .route(
            "/api/v4/data_retention/policies/{policy_id}",
            partially_migrated_with_ids(
                &state,
                get(data_retention::get_policy)
                    .patch(data_retention::patch_policy)
                    .delete(data_retention::delete_policy),
            ),
        )
        .route(
            "/api/v4/data_retention/policies/{policy_id}/teams",
            partially_migrated_with_ids(
                &state,
                get(data_retention::get_teams_for_policy)
                    .post(data_retention::add_teams_to_policy)
                    .delete(data_retention::remove_teams_from_policy),
            ),
        )
        .route(
            "/api/v4/data_retention/policies/{policy_id}/channels",
            partially_migrated_with_ids(
                &state,
                get(data_retention::get_channels_for_policy)
                    .post(data_retention::add_channels_to_policy)
                    .delete(data_retention::remove_channels_from_policy),
            ),
        )
        // Two segments deeper than `/users/{user_id}`, so they shadow nothing.
        .route(
            "/api/v4/users/{user_id}/data_retention/team_policies",
            partially_migrated_with_ids(&state, get(data_retention::get_team_policies_for_user)),
        )
        .route(
            "/api/v4/users/{user_id}/data_retention/channel_policies",
            partially_migrated_with_ids(&state, get(data_retention::get_channel_policies_for_user)),
        )
        // ---- schemes (2026-09-07) ----
        //
        // `/schemes` and `/schemes/{scheme_id}` are a literal and its parameterised child, as in
        // gorilla; the two deeper paths shadow nothing. The three write methods are registered
        // beside their reads because their whole behaviour on an unlicensed server — a 501 with
        // the route's own error id, after the body and id checks — is ported; a licensed one is
        // forwarded from inside the handler rather than by omitting the method here, so the
        // 400-before-501 ordering stays ours to reproduce.
        .route(
            "/api/v4/schemes",
            partially_migrated(get(schemes::get_schemes).post(schemes::create_scheme)),
        )
        .route(
            "/api/v4/schemes/{scheme_id}",
            partially_migrated_with_ids(
                &state,
                get(schemes::get_scheme).delete(schemes::delete_scheme),
            ),
        )
        .route(
            "/api/v4/schemes/{scheme_id}/patch",
            partially_migrated_with_ids(&state, put(schemes::patch_scheme)),
        )
        .route(
            "/api/v4/schemes/{scheme_id}/teams",
            partially_migrated_with_ids(&state, get(schemes::get_teams_for_scheme)),
        )
        .route(
            "/api/v4/schemes/{scheme_id}/channels",
            partially_migrated_with_ids(&state, get(schemes::get_channels_for_scheme)),
        )
        // ---- system, usage and permissions (2026-09-07) ----
        //
        // Nine of these ten paths have no parameters at all, so nothing about them can collide
        // with the user/team/channel tree above; `/api/v4/audits` is a literal directly under the
        // API root, where Go registers it too (`BaseRoutes.APIRoot`, system.go:44).
        //
        // `/system/ping` is the one route here registered without `AuthenticatedSession`: Go uses
        // `api.APIHandler`, so it answers an anonymous request.
        .route(
            "/api/v4/system/ping",
            partially_migrated(get(system::get_system_ping)),
        )
        .route(
            "/api/v4/system/timezones",
            partially_migrated(get(system::get_supported_timezones)),
        )
        .route(
            "/api/v4/system/schema/version",
            partially_migrated(get(system::get_applied_schema_migrations)),
        )
        // Go registers a GET and a POST on this path; only the GET is migrated, so the POST falls
        // to `partially_migrated`'s fallback and Go still completes onboarding. That is not a
        // deferral of convenience: `completeOnboarding` installs marketplace plugins in
        // goroutines, and there is no plugin host here.
        .route(
            "/api/v4/system/onboarding/complete",
            partially_migrated(get(system::get_onboarding)),
        )
        .route(
            "/api/v4/cluster/status",
            partially_migrated(get(system::get_cluster_status)),
        )
        .route(
            "/api/v4/audits",
            partially_migrated(get(audits::get_audits)),
        )
        // `api4/user.go`'s two remaining literal-path reads. Both sit under `/api/v4/users`
        // beside `{user_id}`, and gorilla matches literals before parameters — so `auth_data`
        // and `invalid_emails` are these handlers and never `getUser`. axum's router agrees, and
        // `mux_segments_or_forward` is not involved because neither path has a parameter.
        .route(
            "/api/v4/users/auth_data",
            partially_migrated(get(users::get_user_by_auth_data)),
        )
        .route(
            "/api/v4/users/invalid_emails",
            partially_migrated(get(users::get_users_with_invalid_emails)),
        )
        // `api4/report.go`'s two reads. `/reports/users/count` is registered as its own literal
        // path and not as a parameter of `/reports/users`, exactly as Go's
        // `BaseRoutes.Reports.Handle("/users/count", …)` is — so `POST /reports/users/export`
        // and `POST /reports/posts`, which share the prefix and are not migrated, still forward.
        .route(
            "/api/v4/reports/users",
            partially_migrated(get(reports::get_users_for_reporting)),
        )
        .route(
            "/api/v4/reports/users/count",
            partially_migrated(get(reports::get_user_count_for_reporting)),
        )
        // `api4/group.go`'s eight remaining reads, every one of which opens with
        // `requireLicense`. `{syncable_type}` is gorilla's `teams|channels` alternation and is not
        // id-shaped, so the handler carries that charset itself; `members` and `stats` are
        // literals beside it and axum prefers a literal, which is the order gorilla registers
        // them in too.
        // `/names` is a **literal** beside `{group_id:[A-Za-z0-9]+}`, and `names` matches that
        // class — so in Go the method picks the handler: `POST` is `getGroupsByNames`, `DELETE` is
        // `deleteGroup` with `group_id = "names"`.
        //
        // **A static route shadows its parameterised sibling for every method**, because axum
        // prefers the literal segment and does not backtrack across method routers. Registering
        // `/names` for `POST` alone would therefore have handed `GET /api/v4/groups/names` — which
        // `groups::get_group` served until now — to the fallback. The two methods gorilla actually
        // routes at that path are re-claimed here; `PUT` and the rest stay forwarded, which is
        // also what gorilla does with them. Measured in
        // `parity::group_writes::groups_names_is_a_literal_for_the_post_only`.
        .route(
            "/api/v4/groups/names",
            partially_migrated(
                post(groups::get_groups_by_names)
                    .get(groups::get_group_named_names)
                    .delete(groups::delete_group_named_names),
            ),
        )
        .route(
            "/api/v4/groups/{group_id}",
            partially_migrated_with_ids(
                &state,
                get(groups::get_group).delete(groups::delete_group),
            ),
        )
        .route(
            "/api/v4/groups/{group_id}/patch",
            partially_migrated_with_ids(&state, put(groups::patch_group)),
        )
        .route(
            "/api/v4/groups/{group_id}/restore",
            partially_migrated_with_ids(&state, post(groups::restore_group)),
        )
        .route(
            "/api/v4/groups/{group_id}/members",
            partially_migrated_with_ids(
                &state,
                get(groups::get_group_members)
                    .post(groups::add_group_members)
                    .delete(groups::delete_group_members),
            ),
        )
        .route(
            "/api/v4/groups/{group_id}/stats",
            partially_migrated_with_ids(&state, get(groups::get_group_stats)),
        )
        .route(
            "/api/v4/groups/{group_id}/{syncable_type}",
            partially_migrated_with_ids(&state, get(groups::get_group_syncables)),
        )
        .route(
            "/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}",
            partially_migrated_with_ids(&state, get(groups::get_group_syncable)),
        )
        // The last three of `InitGroup`'s twenty pairs. Both literals sit at a **fourth** segment
        // under two parameters, where nothing was registered before — so unlike `/groups/names`
        // neither can shadow a served sibling: `/groups/{group_id}/patch` is one parameter and a
        // literal, this is two parameters, a literal and one more segment between them. That is
        // an argument, not evidence, so
        // `parity::group_syncables::every_group_route_this_server_answered_still_answers` re-asks
        // all twenty after these two registrations.
        .route(
            "/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link",
            partially_migrated_with_ids(
                &state,
                post(groups::link_group_syncable).delete(groups::unlink_group_syncable),
            ),
        )
        .route(
            "/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/patch",
            partially_migrated_with_ids(&state, put(groups::patch_group_syncable)),
        )
        .route(
            "/api/v4/channels/{channel_id}/groups",
            partially_migrated_with_ids(&state, get(groups::get_groups_by_channel)),
        )
        .route(
            "/api/v4/teams/{team_id}/groups",
            partially_migrated_with_ids(&state, get(groups::get_groups_by_team)),
        )
        .route(
            "/api/v4/teams/{team_id}/groups_by_channels",
            partially_migrated_with_ids(
                &state,
                get(groups::get_groups_associated_to_channels_by_team),
            ),
        )
        // --- `gated_reads`: ten refusals across eight api4 files. See that module for the table.
        .route(
            "/api/v4/hosted_customer/signup_available",
            partially_migrated(get(gated_reads::handle_signup_available)),
        )
        .route(
            "/api/v4/trial-license/prev",
            partially_migrated(get(gated_reads::get_prev_trial_license)),
        )
        // `APIHandler`, not `APISessionRequired` — no session extractor, deliberately.
        .route(
            "/api/v4/saml/metadata",
            partially_migrated(get(gated_reads::get_saml_metadata)),
        )
        .route(
            "/api/v4/ldap/groups",
            partially_migrated(get(gated_reads::get_ldap_groups)),
        )
        .route(
            "/api/v4/system/support_packet",
            partially_migrated(get(gated_reads::generate_support_packet)),
        )
        .route(
            "/api/v4/custom_profile_attributes/group",
            partially_migrated(get(gated_reads::get_cpa_group)),
        )
        // The other seven routes of `api4/custom_profile_attributes.go`. Registered beside the
        // `/group` read they share a licence contract with, and served only while this
        // installation is unlicensed — see `crate::custom_profile_attributes`.
        .route(
            "/api/v4/custom_profile_attributes/fields",
            partially_migrated(
                get(custom_profile_attributes::list_cpa_fields)
                    .post(custom_profile_attributes::create_cpa_field),
            ),
        )
        .route(
            "/api/v4/custom_profile_attributes/fields/{field_id}",
            partially_migrated_with_ids(
                &state,
                patch(custom_profile_attributes::patch_cpa_field)
                    .delete(custom_profile_attributes::delete_cpa_field),
            ),
        )
        .route(
            "/api/v4/custom_profile_attributes/values",
            partially_migrated(patch(custom_profile_attributes::patch_cpa_values)),
        )
        .route(
            "/api/v4/users/{user_id}/custom_profile_attributes",
            partially_migrated_with_ids(
                &state,
                get(custom_profile_attributes::list_cpa_values)
                    .patch(custom_profile_attributes::patch_cpa_values_for_user),
            ),
        )
        // The four reads of `api4/properties.go` — the *generic* PSAv2 property API, of which the
        // CPA family above is one group's worth pinned to one object type. The five writes in the
        // same file are unregistered and fall through to the proxy.
        //
        // `partially_migrated_with_ids` on all four: `group_name` and `object_type` carry mux
        // patterns of their own (`[a-z][a-z0-9_]*` and `[a-z]+`), so a segment outside them has to
        // reach Go for its 404 rather than our handler for a 400. See `segment_matches_go_mux_for`.
        .route(
            "/api/v4/properties/groups/{group_name}/{object_type}/fields",
            partially_migrated_with_ids(&state, get(properties::get_property_fields)),
        )
        .route(
            "/api/v4/properties/groups/{group_name}/fields/search",
            partially_migrated_with_ids(&state, post(properties::search_property_fields)),
        )
        // The first of the five writes in `api4/properties.go`. `PATCH` on the same path is
        // `patchPropertyField` and stays forwarded, which `partially_migrated`'s method fallback
        // handles; the path itself is one segment deeper than the `fields` collection above, so
        // there is no precedence question with it.
        .route(
            "/api/v4/properties/groups/{group_name}/{object_type}/fields/{field_id}",
            partially_migrated_with_ids(&state, delete(properties::delete_property_field)),
        )
        .route(
            "/api/v4/properties/groups/{group_name}/{object_type}/values/{target_id}",
            partially_migrated_with_ids(&state, get(properties::get_property_values)),
        )
        .route(
            "/api/v4/properties/groups/{group_name}/system/values",
            partially_migrated_with_ids(&state, get(properties::get_system_property_values)),
        )
        // Four segments under `/users`, so it shadows none of the `{user_id}` routes; `APIHandler`
        // again, so no session extractor.
        .route(
            "/api/v4/users/sessions/attributes/manifest",
            partially_migrated(get(gated_reads::get_session_attributes_manifest)),
        )
        .route(
            "/api/v4/oauth/outgoing_connections",
            partially_migrated(get(gated_reads::list_outgoing_oauth_connections)),
        )
        .route(
            "/api/v4/oauth/outgoing_connections/{outgoing_oauth_connection_id}",
            partially_migrated_with_ids(&state, get(gated_reads::get_outgoing_oauth_connection)),
        )
        .route(
            "/api/v4/jobs/{job_id}/download",
            partially_migrated_with_ids(&state, get(gated_reads::download_job)),
        )
        .route(
            "/api/v4/files/{file_id}/link",
            partially_migrated_with_ids(&state, get(gated_reads::get_file_link)),
        )
        // `/files/{file_id}/public` (api4/file.go:41) — outside `/api/`, unauthenticated, and
        // the only route in this family whose **errors are not JSON**: `web.Handler` renders a
        // signed HTML page through `utils.RenderWebAppError`, which needs the server's
        // `AsymmetricSigningKey` ([D-170]). So the handler serves the success path and forwards
        // every failure, including the 403 a stock server gives because `EnablePublicLink` is
        // off. GET and HEAD, both through axum's `get`.
        .route(
            "/files/{file_id}/public",
            partially_migrated_with_ids(&state, get(files::get_public_file)),
        )
        .route(
            "/api/v4/cloud/preview/modal_data",
            partially_migrated(get(gated_reads::get_preview_modal_data)),
        )
        .route(
            "/api/v4/license/load_metric",
            partially_migrated(get(gated_reads::get_license_load_metric)),
        )
        // The four personal-access-token reads. `/users/tokens` and its children are literals
        // under `/users`, so they never reach the `{user_id}` route; `/users/{user_id}/tokens` is
        // the parameterised one and resolves `me` in the handler.
        .route(
            "/api/v4/users/tokens",
            partially_migrated(get(tokens::get_user_access_tokens)),
        )
        .route(
            "/api/v4/users/tokens/non_compliant/count",
            partially_migrated(get(tokens::count_non_compliant_user_access_tokens)),
        )
        .route(
            "/api/v4/users/tokens/{token_id}",
            partially_migrated_with_ids(&state, get(tokens::get_user_access_token)),
        )
        .route(
            "/api/v4/users/{user_id}/tokens",
            partially_migrated_with_ids(
                &state,
                get(tokens::get_user_access_tokens_for_user).post(tokens::create_user_access_token),
            ),
        )
        // The six personal-access-token **writes**. Five are literal children of `/users/tokens`
        // and therefore siblings of the `{token_id}` route above; matchit prefers the literal for
        // **every** method, so registering `POST /users/tokens/revoke` also takes `GET` on that
        // path away from the `{token_id}` handler — which is why each of these goes through
        // `partially_migrated` and forwards its other methods to Go rather than 405-ing.
        .route(
            "/api/v4/users/tokens/revoke",
            partially_migrated(post(tokens::revoke_user_access_token)),
        )
        .route(
            "/api/v4/users/tokens/disable",
            partially_migrated(post(tokens::disable_user_access_token)),
        )
        .route(
            "/api/v4/users/tokens/enable",
            partially_migrated(post(tokens::enable_user_access_token)),
        )
        .route(
            "/api/v4/users/tokens/rotate",
            partially_migrated(post(tokens::rotate_user_access_token)),
        )
        .route(
            "/api/v4/users/tokens/search",
            partially_migrated(post(tokens::search_user_access_tokens)),
        )
        .route(
            "/api/v4/users/tokens/non_compliant/revoke",
            partially_migrated(post(tokens::revoke_non_compliant_user_access_tokens)),
        )
        // `BaseRoutes.Teams.Handle("/invite/{invite_id:[A-Za-z0-9]+}")` (api4/team.go:75) — a
        // literal `invite` sibling of `{team_id}`, and an `APIHandler`, so no session extractor.
        // `invite_id` is not id-shaped, so the id-charset middleware does not apply; the mux class
        // is the same `[A-Za-z0-9]+` and axum's `{invite_id}` is wider, which only matters for a
        // segment Go would have 404'd — and this handler answers 404 for it too, from the store.
        .route(
            "/api/v4/teams/invite/{invite_id}",
            partially_migrated(get(teams::get_invite_info)),
        )
        .route(
            "/api/v4/commands",
            partially_migrated(get(commands::list_commands).post(commands::create_command)),
        )
        // `POST` is **not** registered here, and that is load-bearing: `/api/v4/commands/execute`
        // is a static sibling this router does not carry, so a `POST` to it matches this pattern
        // and reaches the method fallback, which forwards it to Go. Registering `create_command`
        // on `{command_id}` as well would swallow `executeCommand` instead.
        .route(
            "/api/v4/commands/{command_id}",
            partially_migrated_with_ids(
                &state,
                get(commands::get_command)
                    .put(commands::update_command)
                    .delete(commands::delete_command),
            ),
        )
        .route(
            "/api/v4/commands/{command_id}/move",
            partially_migrated_with_ids(&state, put(commands::move_command)),
        )
        .route(
            "/api/v4/commands/{command_id}/regen_token",
            partially_migrated_with_ids(&state, put(commands::regen_command_token)),
        )
        .route(
            "/api/v4/bots",
            partially_migrated(get(bots::get_bots).post(bots::create_bot)),
        )
        // `BaseRoutes.Bot.Handle("")` carries **three** methods in Go (`GET`, `PUT`, and a
        // `DELETE` that does not exist), so the `get` and the `put` are one `MethodRouter` here.
        // Registering them as two `.route` calls on the same path panics at startup.
        .route(
            "/api/v4/bots/{bot_user_id}",
            partially_migrated_with_ids(&state, get(bots::get_bot).put(bots::patch_bot)),
        )
        // One segment deeper than `{bot_user_id}`, so there is no precedence question with the
        // route above — axum matches on the number of segments first.
        .route(
            "/api/v4/bots/{bot_user_id}/disable",
            partially_migrated_with_ids(&state, post(bots::disable_bot)),
        )
        .route(
            "/api/v4/bots/{bot_user_id}/enable",
            partially_migrated_with_ids(&state, post(bots::enable_bot)),
        )
        // `{user_id:[A-Za-z0-9]+}` is the **only** id in this family Go spells with an explicit
        // charset in `InitBot`; the other two inherit it from `BaseRoutes`. Both are id-shaped, so
        // `partially_migrated_with_ids` applies the same rule to each — including to the literal
        // `me`, which is alphanumeric and therefore routed rather than forwarded.
        .route(
            "/api/v4/bots/{bot_user_id}/assign/{user_id}",
            partially_migrated_with_ids(&state, post(bots::assign_bot)),
        )
        .route("/api/v4/jobs", partially_migrated(get(jobs::get_jobs)))
        // `BaseRoutes.Jobs.Handle("/type/{job_type:[A-Za-z0-9_-]+}")` (api4/job.go:28). Two
        // segments deeper than `{job_id}` below, so there is no precedence question; the handler
        // carries its own mux charset, because `job_type` is not id-shaped and the id middleware
        // therefore does not see it.
        .route(
            "/api/v4/jobs/type/{job_type}",
            partially_migrated(get(jobs::get_jobs_by_type)),
        )
        .route(
            "/api/v4/jobs/{job_id}",
            partially_migrated_with_ids(&state, get(jobs::get_job)),
        )
        .route(
            "/api/v4/usage/posts",
            partially_migrated(get(usage::get_posts_usage)),
        )
        .route(
            "/api/v4/usage/storage",
            partially_migrated(get(usage::get_storage_usage)),
        )
        .route(
            "/api/v4/usage/teams",
            partially_migrated(get(usage::get_teams_usage)),
        )
        .route(
            "/api/v4/permissions/ancillary",
            partially_migrated(post(permissions::append_ancillary_permissions_post)),
        )
        // `initChannelJoinRequestRoutes` (api4/channel_join_request.go:18), all seven — and
        // registered here **unconditionally**, unlike Go. The `FeatureFlags.DiscoverableChannels`
        // gate is the first statement of each handler instead, and forwards when it is off, so a
        // dark deployment answers Go's own mux 404 rather than one reproduced here. See
        // [`channel_join_requests`] and [D-153].
        .route(
            "/api/v4/channels/{channel_id}/join_request",
            partially_migrated_with_ids(
                &state,
                post(channel_join_requests::request_join_channel)
                    .get(channel_join_requests::get_my_channel_join_request)
                    .delete(channel_join_requests::withdraw_my_channel_join_request),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/join_requests",
            partially_migrated_with_ids(
                &state,
                get(channel_join_requests::get_channel_join_requests),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/join_requests/count",
            partially_migrated_with_ids(
                &state,
                get(channel_join_requests::count_pending_channel_join_requests),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/join_requests/{request_id}",
            partially_migrated_with_ids(
                &state,
                patch(channel_join_requests::patch_channel_join_request),
            ),
        )
        .route(
            "/api/v4/users/{user_id}/channel_join_requests",
            partially_migrated_with_ids(
                &state,
                get(channel_join_requests::get_my_channel_join_requests),
            ),
        )
        // `api.BaseRoutes.ChannelViews` / `ChannelView` / `ChannelViewPosts` (api4/api.go), all
        // seven registered by `InitView` (api4/view.go:14) — and registered here
        // **unconditionally**, unlike Go. The `FeatureFlags.IntegratedBoards` gate is the first
        // statement of each handler instead, and forwards when it is off, so a dark deployment
        // answers Go's own mux 404 rather than one reproduced here. See [`views`].
        .route(
            "/api/v4/channels/{channel_id}/views",
            partially_migrated_with_ids(
                &state,
                get(views::get_views_for_channel).post(views::create_view),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/views/{view_id}",
            partially_migrated_with_ids(
                &state,
                get(views::get_view)
                    .patch(views::update_view)
                    .delete(views::delete_view),
            ),
        )
        .route(
            "/api/v4/channels/{channel_id}/views/{view_id}/posts",
            partially_migrated_with_ids(&state, get(views::get_posts_for_view)),
        )
        .route(
            "/api/v4/channels/{channel_id}/views/{view_id}/sort_order",
            partially_migrated_with_ids(&state, post(views::update_view_sort_order)),
        )
        // `api.BaseRoutes.APIRoot.Handle("/server_busy", …)` three times (api4/system.go:60-62).
        // The state behind them is in *this* process's memory and Go's is in its own, which is
        // [D-320] — the routes are ported, the divergence is recorded, and the local twins of all
        // three are in `local::router`.
        .route(
            "/api/v4/server_busy",
            partially_migrated(
                get(system::get_server_busy_expires)
                    .post(system::set_server_busy)
                    .delete(system::clear_server_busy),
            ),
        )
        // ---- the config reads (2026-09-11) ----
        //
        // `/config` shares its path with `PUT /config`, which is still Go's, so it must go
        // through `partially_migrated` or the PUT becomes a 405 — see that function's comment.
        .route(
            "/api/v4/config",
            partially_migrated(get(config::get_config)),
        )
        // `APIHandler`, not `APISessionRequired`: an anonymous caller gets the limited map rather
        // than a 401, which is what every client reads before it can log in.
        .route(
            "/api/v4/config/client",
            partially_migrated(get(config::get_client_config)),
        )
        .route(
            "/api/v4/config/environment",
            partially_migrated(get(config::get_environment_config)),
        )
        .fallback(proxy::forward_to_go)
        // Outermost, so it sees every response this server produces — including the proxy's,
        // which it then leaves alone. See [`go_global_headers`].
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            go_global_headers,
        ))
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

    /// The two privacy accessors read **different** settings.
    ///
    /// Both default to `true` and the development stack leaves them there, so a swapped wiring is
    /// invisible to every parity test in the repo — the two servers would agree because the two
    /// values agree. The only way to see it is a configuration where they differ, which is what
    /// this builds. `Sanitize` consults them independently and "names but not emails" is an
    /// ordinary deployment, so the swap is reachable in the real world even though the stack
    /// cannot show it.
    #[tokio::test]
    async fn the_privacy_accessors_do_not_read_the_same_setting() {
        let config = mm_app::config::Config {
            show_full_name: true,
            show_email_address: false,
            ..mm_app::config::Config::default()
        };
        let app = App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            config,
        );
        let state = AppState::new(app, "http://localhost:8065".to_owned());

        assert!(state.show_full_name(), "names are shown");
        assert!(!state.show_email_address(), "emails are not");
    }
}
