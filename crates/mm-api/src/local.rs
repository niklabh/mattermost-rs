//! The local-mode admin API: the same api4 handlers, on a unix domain socket, with no session.
//!
//! Go builds this router in `api4.InitLocal` (api.go:425) and serves it from
//! `Server.startLocalModeServer` (app/server.go:1228). It is a *second* router over the same
//! handlers — 171 of this project's 764 route+method pairs live only here — and it differs from
//! the HTTP one in exactly two ways: the transport is a socket rather than a port, and there is no
//! session to authenticate.
//!
//! # The transport
//!
//! ```text
//! mmctl --local ──▶ mmrs.socket ──┬──▶ handled here (GET /api/v4/system/ping)
//!                                 └──▶ local.socket  (the Go server's own socket)
//! ```
//!
//! The Strangler Fig proxy, over a socket instead of a port. `reqwest` cannot dial a unix socket,
//! so the forward leg here is hyper directly — see [`forward_over_unix`]. Without it the 165
//! local routes this server has *not* migrated would answer 404 to a client that Go answers
//! properly, which is the one thing the proxy exists to prevent.
//!
//! Go's own socket handling, mirrored in [`bind`]:
//!
//! - **`os.RemoveAll(socket)` first** (server.go:1234). A unix socket outlives the process that
//!   bound it — `bind(2)` fails with `EADDRINUSE` on the stale file — so a server that did not
//!   unlink it would never restart after a crash.
//! - **`net.Listen("unix", socket)`**, then **`os.Chmod(socket, 0600)`** (server.go:1242). The
//!   mode is the whole access-control story for an API with no authentication: owner-only. It is
//!   set *after* the bind, not with an umask, so there is a window in which the socket exists at
//!   the process umask — a Go race this port reproduces rather than improves on, because
//!   diverging here would mean the two servers disagree about who can reach an unrestricted API.
//!
//! # The authentication model, which is the part a wrong guess makes insecure
//!
//! `api.APILocal` (api4/handlers.go:200) builds a `web.Handler` with `RequireSession: false` and
//! `IsLocal: true`. At handlers.go:361 that becomes:
//!
//! ```go
//! isLocalOrigin := !strings.Contains(r.RemoteAddr, ":")
//! if *c.App.Config().ServiceSettings.EnableLocalMode && isLocalOrigin {
//!     c.AppContext = c.AppContext.WithSession(&model.Session{Local: true})
//! } else if !isLocalOrigin {
//!     c.Err = … "api.context.local_origin_required.app_error" … 401
//! }
//! ```
//!
//! So the session is a **zero-valued `model.Session` with `Local: true` and nothing else** — no
//! id, no `UserId`, no roles. `Session.IsUnrestricted()` returns `s.Local` (model/session.go:103)
//! and `SessionHasPermissionTo` short-circuits on it (app/authorization.go:19), so **every
//! permission check passes** and no role is ever consulted. That is the entire model: the socket
//! is the credential.
//!
//! Two consequences worth stating because they are easy to get wrong:
//!
//! - A local handler that reads `Session().UserId` reads the **empty string**, not an admin's id.
//!   `GET /api/v4/users/me` on the socket is a 400 about `user_id`, not a user — measured against
//!   the Go server. Any local route whose Go handler does something different from its HTTP twin
//!   does it *because* of this, and `*_local.go` exists to hold those differences.
//! - The checks that are **not** `session_has_permission_*` do not get the shortcut.
//!   `App::has_permission_to` (mm-app/src/authorization.rs:171) loads roles from the `Users` row
//!   and has no `is_unrestricted` branch, so a local session asking through that path is asking
//!   about the empty user id and is denied. Go behaves identically.
//!
//! **What keeps a TCP client out is structural, not a check.** Go's `isLocalOrigin` test exists
//! because `web.Handler` is one type serving both routers and a misregistration would otherwise
//! be silently unauthenticated. Here the local router is a distinct `Router` value that
//! [`crate::main`] hands only to [`serve`], which takes a [`tokio::net::UnixListener`] and
//! nothing else — there is no code path that binds it to a port. The test module holds that down
//! by asserting the TCP router does not answer the routes this one adds.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, Query, RawPathParams, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};
use mm_app::license::LicenseState;
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::{AppState, bots, roles, status, system};

/// The path of the Go server's own local socket, carried as a request extension.
///
/// An `Extension` rather than a field on [`AppState`]: it is meaningful only on the local router,
/// and a field on the shared state would invite the HTTP handlers to reach for it.
#[derive(Clone, Debug)]
pub struct GoLocalSocket(pub Arc<PathBuf>);

/// The session `APILocal` installs: zero-valued, `local: true`, therefore unrestricted.
///
/// See the module docs. This is a whole security model in four lines, so it is a named function
/// with a test rather than an inline literal at each call site.
pub fn local_session() -> AuthenticatedSession {
    AuthenticatedSession(Session {
        local: true,
        ..Session::default()
    })
}

/// Mark a local path as partially migrated: registered methods are served here, everything else
/// on that path goes to the Go server's socket.
///
/// The local twin of [`crate::partially_migrated`], and it is just as non-optional: axum matches
/// the path before the method, so registering `POST /api/v4/server_busy` without this would turn
/// the `GET` beside it into a 405 from our router instead of Go's answer.
fn partially_migrated(methods: MethodRouter<AppState>) -> MethodRouter<AppState> {
    methods.fallback(forward_to_go_local)
}

/// [`crate::mux_segments_or_forward`] for the socket: forward, over the **unix socket**, any
/// request whose path segments Go's local mux would not have matched.
///
/// The TCP middleware cannot be reused, and the reason is the one thing that would make this a
/// silent bug: it forwards through [`crate::proxy::forward_to_go`], which dials the Go server's
/// *port*. A local request answered over the port is answered by a **different handler chain** —
/// `APISessionRequired` rather than `APILocal` — so an `mmctl --local` call with a malformed id
/// would come back 401 instead of Go's mux 404.
///
/// `role_name` is here and not in the TCP table because on that side
/// [`crate::roles::get_role_by_name`] carries its own charset check and its own forward. Handling
/// it here means that check is unreachable on the socket, which is deliberate: a handler that
/// forwards over the port must never run on this router.
async fn local_mux_segments_or_forward(
    Extension(go): Extension<GoLocalSocket>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Response {
    for (name, value) in &params {
        let matched = match name {
            // `{role_name:[a-z0-9_]+}` (api4/api.go) — narrower than the id class.
            "role_name" => {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            }
            n if crate::parameter_is_id_shaped(n) => crate::segment_matches_go_mux(value),
            _ => true,
        };
        if !matched {
            tracing::debug!(
                parameter = name,
                "path segment is outside Go's mux charset; forwarding so Go answers its own 404"
            );
            return forward_over_unix(&go.0, request).await;
        }
    }
    next.run(request).await
}

/// [`partially_migrated`], plus the segment charset check, for a local route with path
/// parameters.
fn partially_migrated_with_ids(
    state: &AppState,
    methods: MethodRouter<AppState>,
) -> MethodRouter<AppState> {
    partially_migrated(methods).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        local_mux_segments_or_forward,
    ))
}

/// Build the local-mode router.
///
/// `go_socket` is the Go server's `ServiceSettings.LocalModeSocketLocation` — the forward target,
/// not our own listening socket. The two must be different paths; [`crate::main`] refuses to
/// start if they are not.
pub fn router(state: AppState, go_socket: PathBuf) -> Router {
    Router::new()
        // `api.BaseRoutes.System.Handle("/ping", api.APILocal(getSystemPing))`
        // (system_local.go:15). The same handler as the HTTP router's, which makes this the
        // cheapest end-to-end proof that the socket works at all.
        .route(
            "/api/v4/system/ping",
            partially_migrated(get(local_get_system_ping)),
        )
        // `system_local.go:22`.
        .route(
            "/api/v4/system/schema/version",
            partially_migrated(get(local_get_applied_schema_migrations)),
        )
        // Three methods on one path (`system_local.go:17-19`), all three migrated — the only
        // place in this router where `partially_migrated`'s fallback is currently unreachable.
        // It stays, because the next method Go adds here would otherwise 405.
        .route(
            "/api/v4/server_busy",
            partially_migrated(
                get(local_get_server_busy_expires)
                    .post(local_set_server_busy)
                    .delete(local_clear_server_busy),
            ),
        )
        // `license_local.go:17`. Not the same handler as the HTTP route — see
        // [`local_get_client_license`].
        .route(
            "/api/v4/license/client",
            partially_migrated(get(local_get_client_license)),
        )
        // ---- `bot_local.go`. Six of its seven pairs; `convert_to_user` is unported on both
        // routers and falls through to the socket. Every handler is the **HTTP one**, unchanged:
        // each gate on this family is a `SessionHasPermissionTo`, which short-circuits on
        // `Session.Local`, so the local answer is the HTTP handler's own with every check passing.
        // Sharing them rather than reimplementing is what keeps the two routers from drifting.
        .route("/api/v4/bots", partially_migrated(get(local_get_bots)))
        .route(
            "/api/v4/bots/{bot_user_id}",
            partially_migrated_with_ids(&state, get(local_get_bot).put(local_patch_bot)),
        )
        .route(
            "/api/v4/bots/{bot_user_id}/disable",
            partially_migrated_with_ids(&state, post(local_disable_bot)),
        )
        .route(
            "/api/v4/bots/{bot_user_id}/enable",
            partially_migrated_with_ids(&state, post(local_enable_bot)),
        )
        .route(
            "/api/v4/bots/{bot_user_id}/assign/{user_id}",
            partially_migrated_with_ids(&state, post(local_assign_bot)),
        )
        // ---- `status_local.go`, both pairs.
        //
        // **`me` is not a user here.** `RequireUserId` rewrites `me` to the session's `UserId`,
        // which on this router is the empty string, so `GET /users/me/status` over the socket is
        // a 400 naming `user_id` — not the caller's status, and not a 401. The handlers already
        // do that rewrite from the session they are handed, so it needs no local-only branch;
        // `local_mode::me_is_not_a_user_on_the_socket` pins it.
        .route(
            "/api/v4/users/{user_id}/status",
            partially_migrated_with_ids(
                &state,
                get(local_get_user_status).put(local_update_user_status),
            ),
        )
        // ---- `role_local.go`. Four of its five pairs; `{role_id}/patch` is unported.
        .route(
            "/api/v4/roles",
            partially_migrated(get(local_get_all_roles)),
        )
        // Registered POST-only, exactly as on the TCP router: gorilla matched `{role_id}` first,
        // so a **GET** of `/roles/names` is a `getRole` call with `role_id = "names"` and 400s.
        // Letting the method fallback forward it keeps that Go's answer rather than ours.
        .route(
            "/api/v4/roles/names",
            partially_migrated(post(local_get_roles_by_names)),
        )
        .route(
            "/api/v4/roles/name/{role_name}",
            partially_migrated_with_ids(&state, get(local_get_role_by_name)),
        )
        .route(
            "/api/v4/roles/{role_id}",
            partially_migrated_with_ids(&state, get(local_get_role)),
        )
        // `srv.LocalRouter.Handle("/api/v4/{anything:.*}", api.Handle404)` (api.go:527) is Go's
        // own fallback; ours forwards instead, so an unmigrated local route is answered by the Go
        // process rather than 404'd by this one.
        .fallback(forward_to_go_local)
        // The same security headers `web.Handler.ServeHTTP` sets on every API response. The local
        // socket goes through the identical handler in Go, so it gets the identical headers —
        // verified against the running server: `Expires: 0` on the GET, absent on the POST.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::go_global_headers,
        ))
        .layer(Extension(GoLocalSocket(Arc::new(go_socket))))
        .with_state(state)
}

/// Port of `getSystemPing` reached through `APILocal` (system_local.go:15).
///
/// Identical logic to the HTTP route — it is literally the same Go function — differing only in
/// where an unanswerable ping is forwarded to.
#[tracing::instrument(skip_all, fields(forwarded))]
async fn local_get_system_ping(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    Query(params): Query<system::PingParams>,
    request: Request,
) -> Response {
    match system::ping_answer(&state, &params).await {
        Ok(Some(response)) => response,
        Ok(None) => forward_over_unix(&go.0, request).await,
        Err(err) => err.into_response(),
    }
}

/// Port of `getAppliedSchemaMigrations` reached through `APILocal` (system_local.go:22).
///
/// The permission check inside is `SessionHasPermissionToAny(SysconsoleReadPermissions)`, which a
/// local session passes without a role lookup. It is still *called*, exactly as in Go — the
/// handler is shared, not reimplemented — so the two routers cannot drift apart.
async fn local_get_applied_schema_migrations(
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    system::get_applied_schema_migrations(State(state), local_session()).await
}

/// Port of `getServerBusyExpires` reached through `APILocal` (system_local.go:18).
async fn local_get_server_busy_expires(
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    system::get_server_busy_expires(State(state), local_session()).await
}

/// Port of `setServerBusy` reached through `APILocal` (system_local.go:17).
async fn local_set_server_busy(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    system::set_server_busy(State(state), local_session(), request).await
}

/// Port of `clearServerBusy` reached through `APILocal` (system_local.go:19).
async fn local_clear_server_busy(State(state): State<AppState>) -> Result<Response, ApiError> {
    system::clear_server_busy(State(state), local_session()).await
}

/// Port of `localGetClientLicense` (license_local.go:104).
///
/// # Not the same handler as the HTTP route, and the difference is a permission check
///
/// `getClientLicense` (license.go:31) branches on `read_license_information` between the full
/// `ClientLicense()` map and the sanitized one. `localGetClientLicense` has **no branch**: it
/// always returns the full map, which is the local session's unrestricted status expressed as a
/// missing `if` rather than as a check that passes. Reusing the HTTP handler would have been
/// indistinguishable *here* — on an unlicensed server both maps are `{"IsLicensed":"false"}`, and
/// a licensed one is forwarded — but it would encode the wrong reason, and the difference becomes
/// visible the moment the licensed path stops being forwarded.
///
/// The two 400s and their order are the HTTP route's; see [`crate::license::get_client_license`],
/// which this deliberately mirrors rather than calls.
#[tracing::instrument(skip_all, fields(format, licensed))]
async fn local_get_client_license(
    State(state): State<AppState>,
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    let format = crate::channels::query_first(request.uri().query(), "format").unwrap_or_default();
    tracing::Span::current().record("format", &format);

    if format.is_empty() {
        return ApiError::from(AppError::new(
            "localGetClientLicense",
            "api.license.client.old_format.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }
    if format != "old" {
        return ApiError::invalid_param("format").into_response();
    }

    let state_of_licence = match state.app.license_state().await {
        Ok(state_of_licence) => state_of_licence,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("licensed", state_of_licence == LicenseState::Licensed);

    if state_of_licence == LicenseState::Licensed {
        return forward_over_unix(&go.0, request).await;
    }

    match serde_json::to_vec(&LicenseState::unlicensed_client_license()) {
        // `model.MapToJSON` + `w.Write` — no encoder, so **no trailing newline**.
        Ok(body) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the client licence");
            ApiError::from(AppError::new(
                "localGetClientLicense",
                "api.license.client.app_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `getBots` through `APILocal` (bot_local.go:16).
async fn local_get_bots(
    state: State<AppState>,
    query: RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    bots::get_bots(state, query, headers, local_session()).await
}

/// `getBot` through `APILocal` (bot_local.go:9).
///
/// The handler's three-armed permission block collapses to its first arm here —
/// `read_others_bots` passes unconditionally — so the socket sees every bot, including one owned
/// by nobody. Its `bot.OwnerId == session.UserId` arm is the only branch that could compare
/// against the empty local user id, and it is unreachable once the first arm has passed.
async fn local_get_bot(
    state: State<AppState>,
    path: UrlPath<String>,
    query: RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    bots::get_bot(state, path, query, headers, local_session()).await
}

/// `patchBot` through `APILocal` (bot_local.go:10).
async fn local_patch_bot(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    bots::patch_bot(state, path, local_session(), request).await
}

/// `disableBot` through `APILocal` (bot_local.go:11).
async fn local_disable_bot(state: State<AppState>, path: UrlPath<String>) -> Response {
    bots::disable_bot(state, path, local_session()).await
}

/// `enableBot` through `APILocal` (bot_local.go:12).
async fn local_enable_bot(state: State<AppState>, path: UrlPath<String>) -> Response {
    bots::enable_bot(state, path, local_session()).await
}

/// `assignBot` through `APILocal` (bot_local.go:14).
///
/// `me` in the `{user_id}` segment rewrites to the session's user id, which is empty here, so
/// `POST /bots/{id}/assign/me` over the socket is a 400 naming `user_id`. The segment is
/// alphanumeric, so the mux charset check routes it rather than forwarding it — the 400 is ours
/// to produce, and it is Go's.
async fn local_assign_bot(state: State<AppState>, path: UrlPath<(String, String)>) -> Response {
    bots::assign_bot(state, path, local_session()).await
}

/// `getUserStatus` through `APILocal` (status_local.go:9).
async fn local_get_user_status(
    state: State<AppState>,
    path: UrlPath<String>,
) -> Result<Response, ApiError> {
    status::get_user_status(state, path, local_session()).await
}

/// `updateUserStatus` through `APILocal` (status_local.go:10).
///
/// The handler's own gate is `SessionHasPermissionToUser`, which the local session passes for any
/// target — so the socket can set anyone's status, which is what `mmctl` uses it for. What it
/// cannot do is say `me`.
async fn local_update_user_status(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    status::update_user_status(state, local_session(), path, request).await
}

/// `getAllRoles` through `APILocal` (role_local.go:9).
async fn local_get_all_roles(state: State<AppState>) -> Response {
    roles::get_all_roles(state, local_session()).await
}

/// `getRole` through `APILocal` (role_local.go:10).
async fn local_get_role(state: State<AppState>, path: UrlPath<String>) -> Response {
    roles::get_role(state, path, local_session()).await
}

/// `getRoleByName` through `APILocal` (role_local.go:11).
///
/// The handler carries its own `[a-z0-9_]+` check with a **TCP** forward behind it. That branch
/// is dead on this router: [`local_mux_segments_or_forward`] tests the same charset first and
/// forwards over the socket. It is called rather than reimplemented so the two routers cannot
/// disagree about the rest of the handler; the dead branch is the price and it is written down
/// here rather than left for a reader to find.
async fn local_get_role_by_name(
    state: State<AppState>,
    path: UrlPath<String>,
    request: Request,
) -> Response {
    roles::get_role_by_name(state, path, local_session(), request).await
}

/// `getRolesByNames` through `APILocal` (role_local.go:12).
async fn local_get_roles_by_names(
    state: State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    roles::get_roles_by_names(state, local_session(), request).await
}

/// The local router's fallback: forward to the Go server's socket.
///
/// An axum handler so it can sit in `Router::fallback` and in [`partially_migrated`]; the work is
/// [`forward_over_unix`].
async fn forward_to_go_local(
    Extension(go): Extension<GoLocalSocket>,
    request: Request,
) -> Response {
    forward_over_unix(&go.0, request).await
}

/// Failure modes of the unix-socket forward leg.
#[derive(Debug, thiserror::Error)]
pub enum LocalProxyError {
    /// The socket could not be dialled — usually the Go server is down, or local mode is off in
    /// *its* configuration so it never created the socket at all.
    #[error("could not connect to the Go server's local socket")]
    Connect(#[source] io::Error),

    /// The HTTP/1 exchange itself failed.
    #[error("the local-socket request failed")]
    Http(#[source] hyper::Error),

    /// The client's body could not be read before it was forwarded.
    #[error("could not read the request body")]
    Body(#[source] axum::Error),
}

/// Forward one request to the Go server over its unix socket and return its response verbatim.
///
/// # Why this is hyper and not `reqwest`
///
/// `reqwest` has no unix-socket transport, and the crate is pinned with `default-features =
/// false` precisely so the proxy does not rewrite what it forwards. Rather than add a second HTTP
/// client, this drives `hyper::client::conn::http1` over a [`tokio::net::UnixStream`] directly:
/// one connection per request, no pool. That is the right shape here — the local API is an admin
/// interface measured in requests per minute, and a pool keyed by a path would be state to
/// invalidate when the Go server restarts and replaces the socket inode.
///
/// # What is and is not copied
///
/// Headers go through untouched **except** the hop-by-hop set, for the same reason as the TCP
/// proxy ([`crate::proxy`]): they describe one connection, and a forwarded `Content-Length` or
/// `Transfer-Encoding` contradicts the body actually written. Unlike the TCP proxy this does not
/// need to rewrite a URL — the request line is forwarded as it arrived, since a unix socket has no
/// authority component to get wrong.
///
/// The `Host` header is whatever the client sent. Go's mux does not route on it and neither
/// server checks it, so `mmctl`'s `http://localhost` and curl's `--unix-socket` spelling both
/// work; this preserves rather than invents one.
#[tracing::instrument(skip_all, fields(method = %request.method(), path = request.uri().path(), upstream_status))]
pub async fn forward_over_unix(socket: &Path, request: Request) -> Response {
    match send_over_unix(socket, request).await {
        Ok(response) => {
            tracing::Span::current().record("upstream_status", response.status().as_u16());
            response
        }
        Err(err) => {
            // The Go server being unreachable is the migration's most consequential failure: every
            // unmigrated local route is down. Error, not warn — same judgement as the TCP proxy.
            tracing::error!(error = %err, socket = %socket.display(), "forward to the Go server's local socket failed");
            (
                StatusCode::BAD_GATEWAY,
                [("x-mmrs-served-by", "rust")],
                format!("{err}\n"),
            )
                .into_response()
        }
    }
}

/// [`forward_over_unix`] without the error rendering, so callers — including the tests, which use
/// it as a client — can see what went wrong.
pub async fn send_over_unix(socket: &Path, request: Request) -> Result<Response, LocalProxyError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(LocalProxyError::Connect)?;

    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(LocalProxyError::Http)?;

    // The connection task drives the socket while the response is read. It ends when the response
    // body is complete; an error here is reported through the response future, so logging it at
    // debug rather than propagating is not swallowing anything.
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::debug!(error = %err, "local-socket connection closed");
        }
    });

    let (mut parts, body) = request.into_parts();
    parts.headers = forwardable(&parts.headers);
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(LocalProxyError::Body)?;

    let upstream = sender
        .send_request(hyper::Request::from_parts(
            parts,
            http_body_util::Full::new(body),
        ))
        .await
        .map_err(LocalProxyError::Http)?;

    let (parts, body) = upstream.into_parts();
    Ok(Response::from_parts(parts, Body::new(body)))
}

/// Copy headers, dropping the ones that belong to a single connection.
///
/// The same list as [`crate::proxy`]'s, minus `accept-encoding`: that entry exists there because
/// `reqwest` applies its own decompression policy, and hyper at this level applies none — so
/// dropping it would silently deny the client a compressed answer Go was willing to give.
fn forwardable(headers: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    const HOP_BY_HOP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        // Set by the client library from the body actually sent; carrying the inbound value risks
        // contradicting it.
        "content-length",
    ];

    let mut out = axum::http::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !HOP_BY_HOP
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name.as_str()))
        {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Bind the local socket, reproducing `startLocalModeServer`'s three steps (server.go:1233).
///
/// Returns the listener rather than serving, so a caller can report a bind failure before
/// announcing itself as started.
pub async fn bind(socket: &Path) -> io::Result<tokio::net::UnixListener> {
    // `os.RemoveAll(socket)`. A socket file outlives its process, so without this a server that
    // did not shut down cleanly can never restart. `NotFound` is the ordinary case.
    match tokio::fs::remove_file(socket).await {
        Ok(()) => tracing::warn!(
            socket = %socket.display(),
            "removed a stale local socket; a previous server did not shut down cleanly"
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let listener = tokio::net::UnixListener::bind(socket)?;

    // `os.Chmod(socket, 0600)`. With no session to authenticate, this *is* the access control.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).await?;
    }

    Ok(listener)
}

/// Serve `router` on `listener` until the process ends.
pub async fn serve(listener: tokio::net::UnixListener, router: Router) -> io::Result<()> {
    axum::serve(listener, router).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole authentication model, asserted rather than assumed.
    ///
    /// `Local: true` and **nothing else**: an id or a `UserId` here would be a fabricated
    /// identity that audit records and `Session().UserId` reads would pick up, and roles would
    /// make the unrestricted shortcut untestable by making the role check pass too.
    #[test]
    fn the_local_session_is_unrestricted_and_otherwise_empty() {
        let session = local_session().0;
        assert!(session.local, "APILocal sets Local: true");
        assert!(
            session.is_unrestricted(),
            "IsUnrestricted() is Local, so every permission check short-circuits"
        );
        assert_eq!(
            session.user_id, "",
            "there is no user behind a local request"
        );
        assert_eq!(session.id, "", "and no session row");
        assert_eq!(session.roles, "", "and no roles to consult");
        assert!(
            session.get_user_roles().is_empty(),
            "a role check reaching past the shortcut finds nothing to grant"
        );
    }

    /// A default session is *not* local — the flag is the whole difference, so a port that set it
    /// unconditionally would hand every HTTP caller an unrestricted session.
    #[test]
    fn an_ordinary_session_is_not_unrestricted() {
        assert!(!Session::default().is_unrestricted());
    }

    /// Hop-by-hop headers are dropped and everything else survives, `accept-encoding` included.
    #[test]
    fn the_forward_leg_drops_only_the_connection_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("connection", "keep-alive".parse().expect("valid"));
        headers.insert("content-length", "12".parse().expect("valid"));
        headers.insert("accept-encoding", "gzip".parse().expect("valid"));
        headers.insert("x-request-id", "abc".parse().expect("valid"));

        let out = forwardable(&headers);
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("content-length"));
        assert_eq!(
            out.get("accept-encoding").and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "hyper applies no decompression of its own, so the client's preference is Go's to honour"
        );
        assert_eq!(
            out.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("abc")
        );
    }

    /// [`bind`] unlinks a stale socket file, and the result is owner-only.
    ///
    /// Both halves in one test because the second is what makes the first safe to do: unlinking
    /// somebody else's socket would be a denial of service if the mode let somebody else have
    /// one here.
    #[tokio::test]
    async fn bind_replaces_a_stale_socket_and_locks_it_down() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mmrs-local-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("temp dir");
        let socket = dir.join("stale.socket");

        // A stale socket, exactly as a crashed process leaves one.
        let first = bind(&socket).await.expect("first bind");
        drop(first);
        assert!(socket.exists(), "the file outlives the listener");

        let second = bind(&socket).await.expect("rebinding over the stale file");
        let mode = tokio::fs::metadata(&socket)
            .await
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "os.Chmod(socket, 0600)");

        drop(second);
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// The forward leg fails cleanly, and **quickly**, when there is no socket there.
    ///
    /// A connect to a missing unix path is refused by the kernel immediately — there is no
    /// timeout to wait out, which is the property that keeps this out of the slow-test category
    /// CLAUDE.md warns about. The assertion is that it stays that way.
    #[tokio::test]
    async fn a_missing_socket_is_a_502_and_not_a_hang() {
        let missing = std::env::temp_dir().join("mmrs-local-nothing-here.socket");
        tokio::fs::remove_file(&missing).await.ok();

        let started = std::time::Instant::now();
        let request = Request::builder()
            .uri("/api/v4/system/ping")
            .body(Body::empty())
            .expect("request builds");
        let response = forward_over_unix(&missing, request).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a refused connect must not sit on a timeout"
        );
    }
}
