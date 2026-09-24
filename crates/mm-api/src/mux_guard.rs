//! gorilla/mux's two decisions that come before any route is matched, made ahead of axum's.
//!
//! # 1. The clean-path redirect, on every method
//!
//! `mux.Router.ServeHTTP` compares the request's decoded `URL.Path` with `cleanPath` of it (`.`
//! and `..` resolved, `//` collapsed, a trailing slash kept) and, when they differ, answers `301`
//! to the clean path before matching anything (mux.go:176). The web client's fallback already
//! did this for a path no axum route claimed; a path one **did** claim — `/api/v4/users/..` is
//! `{user_id}` = `..` to axum — reached a handler or was forwarded, and the forward leg's URL
//! parser resolves dot segments, so Go was asked for the clean path and answered 404 where it
//! answers a client 301. Measured: `GET /api/v4/users/..` was Go `301 → /api/v4`, ours `404`.
//!
//! # 2. `HEAD` on the api4 tree: Go registers it on three routes
//!
//! Every api4 route is registered with `.Methods(...)`, and gorilla adds nothing to that list: a
//! route registered `.Methods("GET")` does not match `HEAD`. The only api4 registrations that
//! name `HEAD` are the three file reads under `BaseRoutes.File` (api4/file.go:35-38) — the file
//! itself, `/thumbnail` and `/preview`. (`getPublicFile` is `/files/{id}/public` under
//! `BaseRoutes.Root`, outside `/api/v4`; the web client's catch-all and the plugin subrouter,
//! which also take `HEAD`, are outside it too.) The local-mode router registers none.
//!
//! So a `HEAD` to any other `/api/v4/…` path matches no api4 route by method and falls through to
//! the catch-all both routers register last, `"/api/v4/{anything:.*}"` → `api.Handle404`
//! (api4/api.go:418, :527), which has no `.Methods` and so matches. **It is never a 405**:
//! gorilla answers `MethodNotAllowed` only when no route matches at all, and the catch-all always
//! does (a later full match clears the method mismatch — mux.go `Route.Match`). The answer is
//! `web.Handle404`'s JSON `api.context.404.app_error` with its `detailed_error` quoting the path,
//! written by a bare `http.HandlerFunc`: no security headers, no `gzhttp`, no request id — only
//! `Content-Type` and the length. Measured on the stack: `HEAD /api/v4/system/ping` is `404`,
//! `Content-Type: application/json`, `Content-Length: 256`.
//!
//! axum's `get()` answers `HEAD` with the `GET` handler, and `websocket`'s method router answers
//! it `405`, so without this every served `GET` answered `HEAD` with the handler's status and
//! headers ([D-1110]). Registering `GET` without `HEAD` on 400-odd routes is a change nobody would
//! keep: the next route registered with `get()` would bring it back. One guard keyed on the three
//! routes that do take `HEAD` is the whole rule, and it is gorilla's rule.
//!
//! # What it forwards
//!
//! What it cannot answer exactly, which is then Go's own answer:
//!
//! - a `HEAD` under a configured subpath, where Go's root router redirects `/api/v4/…` into the
//!   subpath and `IsAPICall` measures from it;
//! - a `HEAD` whose path is not UTF-8 once decoded, whose `detailed_error` Go's encoder would
//!   rewrite. Every other such request is left to the router, as before.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::Response;
use mm_model::go_url::{GoUrl, parse_request_uri};

use crate::{AppState, gzhttp, proxy, web_static};

/// What the guard does with one request.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Not a `HEAD` into the api4 tree, or one gorilla routes to a handler: the router decides.
    Route,
    /// A `HEAD` this guard cannot answer exactly; Go answers it.
    Forward,
    /// gorilla's clean-path redirect, to this path.
    Redirect(Box<GoUrl>, String),
    /// `Handle404`, for this decoded path.
    NotFound(String),
}

/// Whether a router registers `HEAD` on the api4 file reads. The TCP router does; the local-mode
/// router registers no file route at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReads {
    TakeHead,
    Absent,
}

/// `{file_id:[A-Za-z0-9]+}`, then nothing, `/thumbnail` or `/preview` — the three api4 paths
/// registered `.Methods(http.MethodGet, http.MethodHead)` (api4/file.go:35-38). gorilla's
/// templates are anchored and `StrictSlash` is off, so a trailing slash is not one of them.
fn is_file_read_taking_head(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v4/files/") else {
        return false;
    };
    let (id, tail) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
        && matches!(tail, "" | "/thumbnail" | "/preview")
}

/// The decision for a request, from its method, its request target as sent, the router's file
/// reads and the configured subpath.
fn decide(method: &Method, target: &str, files: FileReads, subpath: &str) -> Decision {
    // Only a path-absolute target; `*` and anything Go's `ParseRequestURI` refuses are left to
    // the router, whose fallback forwards them as it always has.
    if !target.starts_with('/') {
        return Decision::Route;
    }
    let Ok(url) = parse_request_uri(target) else {
        return Decision::Route;
    };
    // gorilla matches the decoded `URL.Path`, so the prefix is tested on that.
    let head_into_api4 = *method == Method::HEAD && url.path.starts_with(b"/api/v4/");
    let Ok(path) = std::str::from_utf8(&url.path) else {
        return if head_into_api4 {
            Decision::Forward
        } else {
            Decision::Route
        };
    };
    let cleaned = web_static::mux_clean_path(path);
    if cleaned != path {
        return Decision::Redirect(Box::new(url), cleaned);
    }
    if !head_into_api4 {
        return Decision::Route;
    }
    if files == FileReads::TakeHead && is_file_read_taking_head(path) {
        return Decision::Route;
    }
    if subpath != "/" {
        return Decision::Forward;
    }
    Decision::NotFound(path.to_owned())
}

/// `Handle404` for a `HEAD`, framed as `net/http` frames it: the length of the body the handler
/// wrote when it fits the 2048-byte buffer, and **no length at all** when it does not — for a
/// `HEAD`, `net/http` writes neither a length nor chunks once the buffer has flushed. Only a path
/// of nearly 2 KB reaches the second case.
fn not_found(state: &AppState, path: &str) -> Response {
    let response = web_static::handle_404(
        state.app.config().default_server_locale.as_str(),
        HeaderMap::new(),
        path,
    );
    use axum::body::HttpBody as _;
    let long = response
        .body()
        .size_hint()
        .exact()
        .is_none_or(|n| n > gzhttp::BUFFER_BEFORE_CHUNKING_SIZE as u64);
    if !long {
        return response;
    }
    let (parts, _) = response.into_parts();
    Response::from_parts(parts, web_static::empty_body(&Method::HEAD))
}

async fn guard(state: AppState, files: FileReads, request: Request, next: Next) -> Response {
    // Owned because the request moves into whichever branch answers it.
    let target = request
        .uri()
        .path_and_query()
        .map_or("/", |pq| pq.as_str())
        .to_owned();
    // Only the `HEAD` branch reads the subpath.
    let subpath = if *request.method() == Method::HEAD {
        state.app.config().subpath()
    } else {
        String::new()
    };
    match decide(request.method(), &target, files, &subpath) {
        Decision::Route => next.run(request).await,
        Decision::Forward => proxy::forward_to_go(State(state), request).await,
        Decision::Redirect(url, cleaned) => {
            web_static::mux_clean_redirect(&url, cleaned, request.method())
        }
        Decision::NotFound(path) => not_found(&state, &path),
    }
}

/// The guard on the TCP router, where the three file reads keep their `HEAD`.
pub(crate) async fn api4_head(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    guard(state, FileReads::TakeHead, request, next).await
}

/// The guard on the local-mode router, which registers no file route: every api4 `HEAD` there is
/// `Handle404` (api4/api.go:527).
pub(crate) async fn local_api4_head(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    guard(state, FileReads::Absent, request, next).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    /// The guard as the router carries it: a served `GET`, a `POST`-only path and the websocket
    /// all answer `HEAD` with the catch-all's 404 — its JSON, its length and no header a handler
    /// would have set — and a method the router forwards comes back with no `Allow`. The Go
    /// upstream is port 1, so a forward is a 502 carrying nothing of Go's.
    #[tokio::test]
    async fn the_router_answers_head_on_the_api4_tree_with_the_catch_all() {
        use axum::http::Request;
        let app = mm_app::App::new(mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://x/y")
                .expect("a lazy pool needs no server"),
        ));
        let state = AppState::new(app, "http://127.0.0.1:1".to_owned());
        let call = |method: Method, path: &'static str| {
            let router = crate::router(state.clone());
            async move {
                router
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(axum::body::Body::empty())
                            .expect("a request"),
                    )
                    .await
                    .expect("the router answers")
            }
        };
        for path in [
            "/api/v4/system/ping",
            "/api/v4/users/login",
            "/api/v4/websocket",
            "/api/v4/users/me",
        ] {
            let response = call(Method::HEAD, path).await;
            assert_eq!(response.status(), 404, "{path}");
            let headers = response.headers();
            assert_eq!(headers["content-type"], "application/json", "{path}");
            let want = web_static::handle_404("en", HeaderMap::new(), path);
            let want_length = axum::body::HttpBody::size_hint(want.body()).exact();
            assert_eq!(
                headers
                    .get("content-length")
                    .and_then(|v| v.to_str().ok()?.parse::<u64>().ok()),
                want_length,
                "{path}"
            );
            for absent in ["allow", "referrer-policy", "vary", "expires", "etag"] {
                assert!(!headers.contains_key(absent), "{path}: {absent}");
            }
        }
        let forwarded = call(Method::DELETE, "/api/v4/system/ping").await;
        assert!(
            !forwarded.headers().contains_key("allow"),
            "a forwarded method"
        );
        // A method the guard does not touch still reaches the handler.
        let get = call(Method::GET, "/api/v4/users/me").await;
        assert_eq!(get.status(), 401);
    }

    const ID: &str = "abcdefghijklmnopqrstuvwxyz";

    fn head(target: &str) -> Decision {
        decide(&Method::HEAD, target, FileReads::TakeHead, "/")
    }

    #[test]
    fn a_head_into_the_api4_tree_is_handle_404_with_the_decoded_path() {
        for target in [
            "/api/v4/system/ping",
            "/api/v4/users/me",
            "/api/v4/websocket",
            "/api/v4/users/login",
            "/api/v4/no-such-route",
            "/api/v4/",
        ] {
            assert_eq!(head(target), Decision::NotFound(target.to_owned()));
        }
        // The query is not part of the path the body quotes.
        assert_eq!(
            head("/api/v4/users?page=1"),
            Decision::NotFound("/api/v4/users".to_owned())
        );
        // gorilla matches the decoded path, and the body quotes it decoded.
        assert_eq!(
            head("/api/v4/users/a%20b"),
            Decision::NotFound("/api/v4/users/a b".to_owned())
        );
        assert_eq!(
            head("/api%2Fv4/system/ping"),
            Decision::NotFound("/api/v4/system/ping".to_owned())
        );
    }

    #[test]
    fn only_the_three_file_reads_keep_their_head_and_only_on_the_tcp_router() {
        for target in [
            format!("/api/v4/files/{ID}"),
            format!("/api/v4/files/{ID}/thumbnail"),
            format!("/api/v4/files/{ID}/preview"),
            "/api/v4/files/A1".to_owned(),
        ] {
            assert_eq!(head(&target), Decision::Route, "{target}");
            assert_eq!(
                decide(&Method::HEAD, &target, FileReads::Absent, "/"),
                Decision::NotFound(target.clone()),
                "local: {target}"
            );
        }
        for target in [
            format!("/api/v4/files/{ID}/info"),
            format!("/api/v4/files/{ID}/link"),
            format!("/api/v4/files/{ID}/"),
            format!("/api/v4/files/{ID}/preview/"),
            format!("/api/v4/files/{ID}/thumbnail/x"),
            "/api/v4/files/a-b".to_owned(),
            "/api/v4/files/a_b/preview".to_owned(),
            "/api/v4/files/".to_owned(),
            "/api/v4/files/search/x".to_owned(),
        ] {
            assert!(
                matches!(head(&target), Decision::NotFound(_)),
                "{target} takes no HEAD in Go"
            );
        }
    }

    #[test]
    fn everything_else_is_left_to_the_router() {
        for method in [Method::GET, Method::POST, Method::OPTIONS, Method::DELETE] {
            assert_eq!(
                decide(&method, "/api/v4/system/ping", FileReads::TakeHead, "/"),
                Decision::Route,
                "{method}"
            );
        }
        for target in [
            "/",
            "/api/v4",
            "/api/v5/x",
            "/api/v3/oauth/x/complete",
            "/static/main.js",
            "/plugins/x/y",
            "/files/abc/public",
            "/xapi/v4/system/ping",
            "*",
        ] {
            assert_eq!(head(target), Decision::Route, "{target}");
        }
    }

    #[test]
    fn a_head_this_cannot_answer_exactly_is_forwarded() {
        // Not UTF-8 once decoded.
        assert_eq!(head("/api/v4/users/%FF"), Decision::Forward);
        assert_eq!(
            decide(&Method::GET, "/api/v4/users/%FF", FileReads::TakeHead, "/"),
            Decision::Route,
            "only a HEAD is forwarded for it"
        );
        // Under a subpath, Go's root router redirects `/api/v4/…` into it.
        assert_eq!(
            decide(
                &Method::HEAD,
                "/api/v4/system/ping",
                FileReads::TakeHead,
                "/mm"
            ),
            Decision::Forward
        );
        assert_eq!(
            decide(&Method::HEAD, "/api/v4/system/ping", FileReads::Absent, ""),
            Decision::Forward
        );
    }

    /// gorilla's clean-path redirect comes before matching, on every method and every path, and
    /// before the subpath.
    #[test]
    fn an_unclean_path_is_redirected_on_every_method() {
        for (target, clean) in [
            ("/api/v4//system/ping", "/api/v4/system/ping"),
            ("/api/v4/system/./ping", "/api/v4/system/ping"),
            ("/api/v4/users/../teams", "/api/v4/teams"),
            ("/api/v4/files/abc/../def", "/api/v4/files/def"),
            ("/api/v4/users/..", "/api/v4"),
            ("/api/v4/users/%2e%2e/x", "/api/v4/x"),
            ("/static//main.js", "/static/main.js"),
            ("/a/./b/", "/a/b/"),
        ] {
            for method in [Method::HEAD, Method::GET, Method::POST, Method::DELETE] {
                for subpath in ["/", "/mm"] {
                    match decide(&method, target, FileReads::TakeHead, subpath) {
                        Decision::Redirect(_, cleaned) => {
                            assert_eq!(cleaned, clean, "{method} {target}")
                        }
                        other => panic!("{method} {target}: {other:?}"),
                    }
                }
            }
        }
        // A trailing slash is kept, so it is clean.
        assert_eq!(
            decide(&Method::GET, "/api/v4/users/me/", FileReads::TakeHead, "/"),
            Decision::Route
        );
    }
}
