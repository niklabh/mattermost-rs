//! The web client: port of the static half of Go's `channels/web` package.
//!
//! ```text
//! GET  /static/plugins/*   pluginHandler   staticFilesHandler(StripPrefix(FileServer(PluginSettings.ClientDirectory)))
//! GET  /static/*           staticHandler   staticFilesHandler(StripPrefix(FileServer(FindDir("client"))))
//! *    /robots.txt         robotsHandler
//! *    /unsupported_browser.js  unsupportedBrowserScriptHandler
//! GET  /{anything:.*}      NewStaticHandler(root)                                   (static.go:28-58)
//! ```
//!
//! Until this file, every one of these went to Go: a browser session through mm-api was 55% Rust
//! with 116 of Go's 121 answers being `/static/` assets and the other few the SPA page itself.
//!
//! # Where this sits
//!
//! It is the TCP router's **fallback**, not a set of routes, because that is where Go's static
//! routes sit too: registered last (`web.New` runs after `api4.Init`), behind the plugin prefix,
//! the api4 tree, the OAuth, SAML, magic-link and webhook routes. [`fallback`] reproduces the part
//! of gorilla's dispatch that decides between those and the static handlers, and forwards
//! everything that is not provably static — see [`classify`]. The local-mode socket router has
//! no static routes in Go and keeps its plain forward.
//!
//! # What gorilla does before any of it
//!
//! `mux.Router.ServeHTTP` **cleans the path and 301s** when cleaning changes it, before matching
//! anything (mux.go:176). So `/static/../x` is never a traversal attempt against the file server;
//! it is a redirect to `/x`, with no headers but `Location`. That is reproduced for every request
//! that reaches the fallback, since Go does it for every request, full stop.
//!
//! # What is fixed at start
//!
//! `InitStatic` and `NewStaticHandler` read the configuration **once**: the webserver mode, the
//! subpath, both directories, and the CSP hashes for the inline scripts `root.html` carries. Go
//! says so ("These values are fixed on server start and intentionally require a restart"), and it
//! matters, because the same startup rewrote `root.html` on disk to match those hashes. Here they
//! are read once, on the first request that needs them ([`StaticSetup`]) — the nearest a lazily
//! configured router gets to "at start".
//!
//! # What is forwarded, and why
//!
//! - The **unsupported-browser** and **unsupported-desktop-app** pages. Both are Go
//!   `html/template` renders of `templates/*.html` with translated strings; this server has
//!   neither the template engine nor the translations. The *decision* is ported
//!   (`CheckClientCompatibility` over the uasurfer port), so only an old Safari or IE is
//!   forwarded; the desktop check forwards any desktop-app agent while
//!   `MinimumDesktopAppVersion` is set, because the Masterminds semver comparison is not ported.
//!   [D-900]
//! - The static handler's **error pages**: a request URI over `MaximumURLLength` and a session
//!   token in `?access_token=`, both of which Go answers with `RenderWebAppError` — a redirect
//!   page whose URL carries an ECDSA signature by the server's key. [D-901]
//! - A multi-range request (`multipart/byteranges`, random boundary — [D-205]), a directory
//!   listing (unreachable: a trailing slash is a 404 before the file server runs), anything not
//!   `GET` or `HEAD`, and every path in a mode or subpath this port cannot resolve.
//!
//! Not ported at all: `UpdateAssetsSubpathFromConfig`, which **rewrites** `root.html`, the
//! manifest and every CSS file in the client directory at startup when the subpath changes. The
//! Go server sharing this directory does it at its own start; a second writer racing it over a
//! directory every numbered stack shares would be worse than none. [D-902]

use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use mm_model::go_path;
use mm_model::go_url::{GoUrl, parse_request_uri};

use crate::AppState;
use crate::serve_content::{FileResponse, build_response, serve_error, set_header};
use crate::{gzhttp, proxy};

/// `robotsTxt` (static.go:26).
const ROBOTS_TXT: &[u8] = b"User-agent: *\nDisallow: /\n";

/// `staticFilesHandler`'s long-lived cache header (static.go:113), and the `no-cache` variant
/// `remote_entry.js` and the SPA page get — a plugin's module-federation entry point and the page
/// that names every hashed bundle must be revalidated, the bundles themselves never.
const CACHE_FOREVER: &str = "max-age=31556926, public";
const CACHE_REVALIDATE: &str = "no-cache, max-age=31556926, public";
/// `notFoundNoCacheResponseWriter` (static.go:141): a 404 from the file server is not cached.
const CACHE_NOT_FOUND: &str = "no-cache, public";

/// `model.TeamSettingsDefaultSiteName` (config.go:143) — also the literal in `root.html`'s
/// `<title>` that `root` replaces.
const DEFAULT_SITE_NAME: &str = "Mattermost";

/// Marks a response this module built as carrying **Go's own header set** for its handler, so
/// [`crate::go_global_headers`] — which adds the API handler's security headers and `Expires` to
/// every locally served response — leaves it alone. Three of these handlers are bare
/// `http.HandlerFunc`s with no security headers at all, and the static one sets its own.
#[derive(Clone, Copy, Debug)]
pub struct WebOwnHeaders;

/// What `InitStatic` and `NewStaticHandler` read once (static.go:29-45, handlers.go:51-72).
#[derive(Debug, Clone)]
pub struct StaticSetup {
    /// `ServiceSettings.WebserverMode`, `"regular"` already folded to `"gzip"` by the config
    /// loader. `"disabled"` means none of these routes exist.
    webserver_mode: String,
    /// `utils.GetSubpathFromConfig` — `/` without one, and `""` when `SiteURL` does not parse
    /// (a Go server with that `SiteURL` refuses to start, so it is forwarded here).
    subpath: String,
    /// `fileutils.FindDir(model.ClientDir)` — the directory, or `./` when there is none.
    static_dir: PathBuf,
    /// `PluginSettings.ClientDirectory`, as configured (relative to the working directory).
    plugin_client_dir: PathBuf,
    /// `utils.GetStaticScriptHashes(subpath, FeatureFlags.EnableConcurrentReact)`.
    csp_sha_directive: String,
    /// `ServiceSettings.EnableTesting` — registers `GET /manualtest` ahead of the catch-all
    /// (api4/api.go:414).
    enable_testing: bool,
    /// This process hosts the plugins (`MMRS_PLUGIN_HOST=rust`), so the plugin HTTP subrouter is
    /// served here ([`crate::plugin_requests`]) rather than forwarded.
    host_plugins: bool,
}

impl StaticSetup {
    /// `ServiceSettings.WebserverMode` as read at start — `gzip` means every handler Go registered
    /// through `APIHandler` or `NewStaticHandler` is wrapped in `gzhttp`.
    pub(crate) fn webserver_mode(&self) -> &str {
        &self.webserver_mode
    }

    fn from_config(config: &mm_model::config::Config, subpath: String) -> Self {
        let service = &config.service_settings;
        let enable_concurrent_react = config
            .feature_flags
            .as_ref()
            .is_some_and(|flags| flags.enable_concurrent_react);
        StaticSetup {
            webserver_mode: normalise_webserver_mode(service.webserver_mode.as_deref()),
            csp_sha_directive: get_static_script_hashes(&subpath, enable_concurrent_react),
            subpath,
            static_dir: mm_app::logs::find_dir(CLIENT_DIR).0,
            plugin_client_dir: PathBuf::from(
                config
                    .plugin_settings
                    .client_directory
                    .as_deref()
                    .unwrap_or("./client/plugins"),
            ),
            enable_testing: service.enable_testing.unwrap_or(false),
            host_plugins: false,
        }
    }
}

/// `model.ClientDir` (client4.go:53).
const CLIENT_DIR: &str = "client";

/// `ServiceSettings.SetDefaults`' `WebserverMode` rule (config.go:843-847): nil is `gzip`, and
/// `regular` is rewritten to `gzip`.
fn normalise_webserver_mode(mode: Option<&str>) -> String {
    match mode {
        None | Some("regular") => "gzip".to_owned(),
        Some(other) => other.to_owned(),
    }
}

/// Port of `getSubpathScript` (subpath.go:25).
fn get_subpath_script(subpath: &str) -> String {
    if subpath.is_empty() || subpath == "/" {
        return String::new();
    }
    format!(
        "window.publicPath='{}/'",
        go_path::join(&[subpath, "static"])
    )
}

/// Port of `getConcurrentReactScript` (subpath.go:37).
fn get_concurrent_react_script(enable_concurrent_react: bool) -> &'static str {
    if enable_concurrent_react {
        "window.enableConcurrentReact=true"
    } else {
        ""
    }
}

/// Port of `GetScriptHash` (subpath.go:46): ` 'sha256-<base64>'`, leading space included, or
/// nothing for no script.
fn get_script_hash(script: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    if script.is_empty() {
        return String::new();
    }
    let digest = sha2::Sha256::digest(script.as_bytes());
    format!(
        " 'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Port of `GetStaticScriptHashes` (subpath.go:60).
pub fn get_static_script_hashes(subpath: &str, enable_concurrent_react: bool) -> String {
    get_script_hash(&get_subpath_script(subpath))
        + &get_script_hash(get_concurrent_react_script(enable_concurrent_react))
}

/// The [`StaticSetup`] for this process, read on first use.
async fn setup(state: &AppState) -> Option<&StaticSetup> {
    state
        .web_setup
        .get_or_try_init(|| async {
            let config = mm_app::config::load_model_config(state.app.store().config()).await?;
            let subpath = state.app.config().subpath();
            let mut setup = StaticSetup::from_config(&config, subpath);
            setup.host_plugins = state.app.plugin_host().hosted();
            Ok::<_, mm_app::config::ConfigError>(setup)
        })
        .await
        .map_err(|err| tracing::warn!(error = %err, "could not read the static setup"))
        .ok()
}

/// Which of Go's handlers a request reaches.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Anything this port does not serve: another Go handler's path, a method the static
    /// routes do not answer, or a shape this port cannot resolve.
    Forward,
    /// The subpath redirects: `/sub` to `/sub/` (static.go:56), and anything outside the
    /// subpath into it (app/server.go:496). Both `http.Redirect`, 302.
    Redirect(String),
    /// `/static/plugins/*` — `true` — or `/static/*`.
    Static {
        plugins: bool,
    },
    Robots,
    UnsupportedBrowserScript,
    /// `GET /manualtest` under `EnableTesting` — `crate::manualtest`. A `HEAD` is `Root`'s:
    /// gorilla's route is `Methods(GET)`.
    ManualTest,
    /// The plugin HTTP subrouter (app/channels.go:239), any method — only under the Rust plugin
    /// host; see [`crate::plugin_requests`].
    Plugin(crate::plugin_requests::PluginRoute),
    Root,
}

/// Gorilla's route choice for a request that no Rust route claimed, restricted to the handlers
/// this module ports. `path` is the full, **decoded**, already-clean path; `raw_query` is kept for
/// the redirects.
///
/// Forwarded prefixes are the routes Go registers ahead of the catch-all: the plugin HTTP
/// subrouter (`/plugins/{plugin_id}`, any method — app/channels.go:239; served here instead,
/// as [`Route::Plugin`], when this process hosts the plugins), `getPublicFile`'s
/// `/files/` subrouter, the api4 tree (`/api/v4` and `/api/v5`, which gorilla's `PathPrefix`
/// matches without a trailing slash), `web.InitOAuth` (`/oauth/…`, `/.well-known/…`,
/// `/api/v3/oauth/…`, `/signup/…/complete`, `/login/…/complete`), `InitSaml` and
/// `InitMagicLink`. The `/oauth/` and `/files/` prefixes are wider than the routes under them —
/// gorilla would hand some of those paths to the catch-all — and forwarding them costs a hop, not
/// an answer. `InitWebhooks`' two routes are `POST` only, so a `GET /hooks/…` **is** the SPA page
/// and is served here.
fn classify(setup: &StaticSetup, method: &Method, path: &str, raw_query: &str) -> Route {
    if setup.subpath.is_empty() {
        return Route::Forward;
    }
    let rel = if setup.subpath == "/" {
        path
    } else if path == setup.subpath {
        // `w.MainRouter.HandleFunc("", …)` — `r.URL.Path += "/"`.
        return Route::Redirect(with_query(&format!("{path}/"), raw_query));
    } else if let Some(rest) = path
        .strip_prefix(setup.subpath.as_str())
        .filter(|rest| rest.starts_with('/'))
    {
        rest
    } else {
        // The root router's `NotFoundHandler`: `path.Join(subpath, r.URL.Path)`.
        return Route::Redirect(with_query(
            &go_path::join(&[&setup.subpath, path]),
            raw_query,
        ));
    };

    // Registered for every method, and ahead of the web client's routes.
    if setup.host_plugins
        && let Some(route) = crate::plugin_requests::plugin_route(rel)
    {
        return Route::Plugin(route);
    }

    if *method != Method::GET && *method != Method::HEAD {
        return Route::Forward;
    }

    const FORWARDED_PREFIXES: [&str; 7] = [
        "/api/v4",
        "/api/v5",
        "/api/v3/oauth/",
        "/plugins/",
        "/files/",
        "/oauth/",
        "/.well-known/oauth-authorization-server",
    ];
    if FORWARDED_PREFIXES.iter().any(|p| rel.starts_with(p)) {
        return Route::Forward;
    }
    if rel == "/login/sso/saml"
        || rel == "/login/one_time_link"
        || is_oauth_complete(rel, "/login/")
        || is_oauth_complete(rel, "/signup/")
    {
        return Route::Forward;
    }
    if setup.enable_testing && rel == "/manualtest" && *method == Method::GET {
        return Route::ManualTest;
    }
    if rel.starts_with("/static/plugins/") {
        return Route::Static { plugins: true };
    }
    if rel.starts_with("/static/") {
        return Route::Static { plugins: false };
    }
    if rel == "/robots.txt" {
        return Route::Robots;
    }
    if rel == "/unsupported_browser.js" {
        return Route::UnsupportedBrowserScript;
    }
    Route::Root
}

/// What kind of `web.Handler` Go's root router hands a request to, when it is one at all — the
/// question `ServeHTTP`'s per-user rate limit (web/handlers.go:288) turns on, for every request
/// this fallback sees, whether it is answered here or forwarded. [D-1150], [D-1151].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlerKind {
    /// `w.APIHandler` and its siblings: the routes `web.InitOAuth`, `InitSaml`,
    /// `InitMagicLink` and `InitWebhooks` register, and `GET /manualtest`.
    Api,
    /// `NewStaticHandler(root)`, the catch-all — `IsStatic`.
    Static,
}

/// `{service:[A-Za-z0-9]+}`.
fn is_service(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// [`HandlerKind`] for a decoded, clean path, in gorilla's match order: the api4 tree (its routes
/// are the API router's, and its catch-all is a bare `HandlerFunc`), the plugin subrouter, the
/// static files, `robots.txt` and `unsupported_browser.js` (plain handlers), then the web routes by
/// method (web/oauth.go:33-53, saml.go:22-23, magic_link.go:15, webhook.go:21-22, and api4's
/// `/manualtest`), and last the catch-all for `GET` and `HEAD` — which is also where a web route's
/// path lands under a method that route does not take.
pub(crate) fn go_handler_kind(
    setup: &StaticSetup,
    method: &Method,
    path: &str,
) -> Option<HandlerKind> {
    let rel = if setup.subpath == "/" {
        path
    } else if setup.subpath.is_empty() {
        return None;
    } else {
        path.strip_prefix(setup.subpath.as_str())
            .filter(|rest| rest.starts_with('/'))?
    };
    if rel.starts_with("/api/v4/") || crate::plugin_requests::plugin_route(rel).is_some() {
        return None;
    }
    let get = *method == Method::GET;
    let post = *method == Method::POST;
    let segments: Vec<&str> = rel.split('/').skip(1).collect();
    let api = match segments.as_slice() {
        ["login", "one_time_link"] => get,
        ["login", "sso", "saml"] => get || post,
        ["oauth", "authorize"] => get || post,
        ["oauth", "deauthorize"] | ["oauth", "access_token"] | ["oauth", "intune"] => post,
        [
            "oauth",
            service,
            "complete" | "login" | "mobile_login" | "signup",
        ] => get && is_service(service),
        ["api", "v3", "oauth", service, "complete"]
        | ["signup", service, "complete"]
        | ["login", service, "complete"] => get && is_service(service),
        ["hooks", "commands", id] | ["hooks", id] => post && is_service(id),
        ["manualtest"] => get && setup.enable_testing,
        _ => get && rel.starts_with("/.well-known/oauth-authorization-server"),
    };
    if api {
        return Some(HandlerKind::Api);
    }
    if setup.webserver_mode == "disabled"
        || rel.starts_with("/static/")
        || rel == "/robots.txt"
        || rel == "/unsupported_browser.js"
    {
        return None;
    }
    (get || *method == Method::HEAD).then_some(HandlerKind::Static)
}

/// `/login/{service:[A-Za-z0-9]+}/complete` and its `/signup/` twin (web/oauth.go:52-53).
fn is_oauth_complete(rel: &str, prefix: &str) -> bool {
    rel.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix("/complete"))
        .is_some_and(|service| {
            !service.is_empty() && service.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn with_query(path: &str, raw_query: &str) -> String {
    if raw_query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{raw_query}")
    }
}

/// Port of gorilla's `cleanPath` (mux.go:464): `path.Clean`, with a trailing slash put back.
pub(crate) fn mux_clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".to_owned();
    }
    let rooted;
    let p = if p.starts_with('/') {
        p
    } else {
        rooted = format!("/{p}");
        rooted.as_str()
    };
    let mut np = go_path::clean(p);
    if p.ends_with('/') && np != "/" {
        np.push('/');
    }
    np
}

/// The TCP router's fallback: gorilla's clean-path redirect, then [`classify`], then the handler.
#[tracing::instrument(skip_all, fields(path = request.uri().path(), route))]
pub async fn fallback(State(state): State<AppState>, request: Request) -> Response {
    let raw_target = request
        .uri()
        .path_and_query()
        .map_or("/", |pq| pq.as_str())
        .to_owned();
    // `*` (the asterisk form) and anything Go's `ParseRequestURI` refuses are Go's to answer:
    // `net/http` handles the first itself and 400s the second before any handler.
    let Some(url) = raw_target
        .starts_with('/')
        .then(|| parse_request_uri(&raw_target).ok())
        .flatten()
    else {
        return proxy::forward_to_go(State(state), request).await;
    };
    let Ok(path) = String::from_utf8(url.path.clone()) else {
        return proxy::forward_to_go(State(state), request).await;
    };

    let cleaned = mux_clean_path(&path);
    if cleaned != path {
        return mux_clean_redirect(&url, cleaned, request.method());
    }

    let Some(setup) = setup(&state).await else {
        return proxy::forward_to_go(State(state), request).await;
    };
    let route = classify(setup, request.method(), &path, &url.raw_query);
    let (parts, body) = request.into_parts();

    // `ServeHTTP`'s per-user step, for a request Go hands to a `web.Handler` — counted here
    // whether this server answers it or forwards it, so that this server's store is the one that
    // decides ([D-1150], [D-1151]); Go's sees a subset and never refuses first. It follows
    // `basicSecurityChecks`, so an over-long URL is not counted.
    let kind = go_handler_kind(setup, &parts.method, &path);
    let verdict = match kind {
        Some(kind) if crate::ratelimit::per_user_enabled(&state).await => {
            per_user_step(&state, setup, kind, &raw_target, &parts).await
        }
        _ => None,
    };
    let verdict = match verdict {
        Some(Ok(verdict)) => Some(verdict),
        Some(Err(refusal)) => return refusal,
        None => None,
    };
    let request = Request::from_parts(parts, body);
    // Cloned (an `Arc` bump) because `setup` borrows the original for the call.
    let mut response =
        fallback_route(state.clone(), setup, route, request, url, raw_target, path).await;
    if let Some(verdict) = verdict {
        crate::ratelimit::append_verdict(&mut response, &verdict);
    }
    response
}

/// The per-user step for a request of `kind`: `None` when it does not run, the verdict when it
/// allows, and the refusal — dressed as `ServeHTTP` dresses it for that kind — when it does not.
async fn per_user_step(
    state: &AppState,
    setup: &StaticSetup,
    kind: HandlerKind,
    raw_target: &str,
    parts: &Parts,
) -> Option<Result<crate::ratelimit::Verdict, Response>> {
    let config = mm_app::config::load_model_config(state.app.store().config())
        .await
        .map_err(|err| tracing::warn!(error = %err, "could not read the configuration"))
        .ok()?;
    let max_url = config.service_settings.maximum_url_length.unwrap_or(2048);
    if i64::try_from(raw_target.len()).unwrap_or(i64::MAX) > max_url {
        return None;
    }
    let verdict = crate::ratelimit::per_user_verdict(state, parts).await?;
    if !verdict.limited {
        return Some(Ok(verdict));
    }
    let request_id = mm_model::utils::new_id();
    let refusal = match kind {
        HandlerKind::Static => {
            let headers = static_handler_headers(state, setup, &config, &request_id)
                .await
                .unwrap_or_default();
            crate::ratelimit::per_user_static_refusal(&verdict, headers)
        }
        HandlerKind::Api => {
            let mut refusal = crate::ratelimit::per_user_refusal(&verdict);
            if let Some(headers) = serve_http_headers(state, &config, &request_id).await {
                for (name, value) in &headers {
                    refusal.headers_mut().insert(name.clone(), value.clone());
                }
            }
            refusal
        }
    };
    Some(Err(refusal))
}

/// [`fallback`] once the per-user step has run: the route's own answer.
async fn fallback_route(
    state: AppState,
    setup: &StaticSetup,
    route: Route,
    request: Request,
    url: GoUrl,
    raw_target: String,
    path: String,
) -> Response {
    // `InitStatic` is skipped when the web server is disabled; the plugin routes are not
    // (they are registered by `NewChannels`).
    if setup.webserver_mode == "disabled" && !matches!(route, Route::Plugin(_)) {
        return proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("route", tracing::field::debug(&route));
    let (parts, body) = request.into_parts();
    if let Route::Plugin(route) = route {
        return crate::plugin_requests::serve(
            &state,
            &setup.subpath,
            route,
            parts,
            body,
            url,
            &raw_target,
            &path,
        )
        .await;
    }
    let answer = match route {
        Route::Forward => None,
        Route::Redirect(location) => Some(http_redirect(&parts.method, &location)),
        Route::Static { plugins } => static_files(setup, plugins, &url, &path, &parts).await,
        Route::Robots => Some(robots()),
        Route::UnsupportedBrowserScript => unsupported_browser_script(&parts).await,
        Route::ManualTest => {
            crate::manualtest::manual_test(&state, setup, &raw_target, &url.raw_query, &parts).await
        }
        Route::Root => root(&state, setup, &raw_target, &path, &parts).await,
        Route::Plugin(_) => None,
    };
    match answer {
        Some(response) => {
            let mut response = head_framing(&parts.method, response);
            response.extensions_mut().insert(WebOwnHeaders);
            response
        }
        None => proxy::forward_to_go(State(state), Request::from_parts(parts, body)).await,
    }
}

/// Gorilla's redirect to the clean path: `Location` and a 301, nothing else — it runs before any
/// handler, so none of their headers are set.
pub(crate) fn mux_clean_redirect(url: &GoUrl, cleaned: String, method: &Method) -> Response {
    let mut target = url.clone();
    target.path = cleaned.into_bytes();
    let mut headers = HeaderMap::new();
    set_header(&mut headers, "location", &target.to_go_string());
    let mut response = build_response(StatusCode::MOVED_PERMANENTLY, headers, empty_body(method));
    response.extensions_mut().insert(WebOwnHeaders);
    response
}

/// An empty body framed as `net/http` frames it: `Content-Length: 0` on a `GET` whose handler
/// wrote nothing, **no length at all** on a `HEAD` (chunkWriter.writeHeader's
/// `!isHEAD || len(p) > 0`).
pub(crate) fn empty_body(method: &Method) -> Body {
    if *method == Method::HEAD {
        Body::from_stream(futures_util::stream::empty::<
            Result<Bytes, std::convert::Infallible>,
        >())
    } else {
        Body::empty()
    }
}

/// A `HEAD` answer whose handler wrote nothing gets **no** `Content-Length` from `net/http`
/// (a `304`, a `412`, a redirect), where hyper would write `0` for an empty body of known size.
/// One whose handler set the header, or wrote a body, keeps it.
pub(crate) fn head_framing(method: &Method, response: Response) -> Response {
    use axum::body::HttpBody as _;
    if *method != Method::HEAD
        || response.headers().contains_key(header::CONTENT_LENGTH)
        || response.body().size_hint().exact() != Some(0)
    {
        return response;
    }
    let (parts, _) = response.into_parts();
    Response::from_parts(parts, empty_body(&Method::HEAD))
}

/// Port of `http.Redirect` for a path-absolute URL and no prior `Content-Type` (server.go): the
/// path is cleaned (trailing slash kept), non-ASCII hex-escaped into `Location`, and a `GET`
/// gets the little HTML body; a `HEAD` gets the content type and no body.
fn http_redirect(method: &Method, url: &str) -> Response {
    let (path, query) = match url.find('?') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    let mut cleaned = go_path::clean(path);
    if path.ends_with('/') && !cleaned.ends_with('/') {
        cleaned.push('/');
    }
    let location = hex_escape_non_ascii(&format!("{cleaned}{query}"));
    let mut headers = HeaderMap::new();
    set_header(&mut headers, "location", &location);
    set_header(&mut headers, "content-type", "text/html; charset=utf-8");
    let body = if *method == Method::GET {
        Body::from(format!(
            "<a href=\"{}\">Found</a>.\n\n",
            html_escape_redirect(&location)
        ))
    } else {
        empty_body(method)
    };
    build_response(StatusCode::FOUND, headers, body)
}

/// `hexEscapeNonASCII` (net/http/server.go).
fn hex_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b >= 0x80 {
            out.push_str(&format!("%{b:02x}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

/// `htmlReplacer` (net/http/server.go) — the five characters `http.Redirect` escapes.
fn html_escape_redirect(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

/// Port of `html.EscapeString` — the same five, used by `getOpenGraphMetaTags`.
fn html_escape_string(s: &str) -> String {
    html_escape_redirect(s)
}

/// `robotsHandler` (static.go:149). A bare `http.HandlerFunc`: no security headers, no gzip, and
/// the content type is `net/http`'s sniff of the body. Its trailing-slash check is unreachable —
/// the route is the exact path, so `/robots.txt/` goes to `root`.
fn robots() -> Response {
    let mut headers = HeaderMap::new();
    set_header(&mut headers, "content-type", "text/plain; charset=utf-8");
    build_response(StatusCode::OK, headers, Body::from(ROBOTS_TXT))
}

/// `unsupportedBrowserScriptHandler` (static.go:161): `http.ServeFile` of
/// `templates/unsupported_browser.js`. No security headers and no gzip — it is not wrapped.
async fn unsupported_browser_script(parts: &Parts) -> Option<Response> {
    let (templates_dir, found) = mm_app::logs::find_dir("templates");
    let dir = if found { templates_dir } else { PathBuf::new() };
    serve_file(
        &dir,
        "/unsupported_browser.js",
        "/unsupported_browser.js",
        "",
        false,
        parts,
        HeaderMap::new(),
    )
    .await
}

/// `staticFilesHandler(http.StripPrefix(prefix, http.FileServer(http.Dir(dir))))`, wrapped in
/// `gzhttp.GzipHandler` in `gzip` mode (static.go:40-45, 103-131).
async fn static_files(
    setup: &StaticSetup,
    plugins: bool,
    url: &GoUrl,
    path: &str,
    parts: &Parts,
) -> Option<Response> {
    let response = static_files_unwrapped(setup, plugins, url, path, parts).await?;
    if setup.webserver_mode != "gzip" {
        return Some(response);
    }
    let accept_encoding = parts
        .headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok());
    match gzhttp::wrap(&parts.method, accept_encoding, response).await {
        Ok(response) => Some(response),
        Err(err) => {
            tracing::error!(error = %err, "compressing a static file failed");
            None
        }
    }
}

async fn static_files_unwrapped(
    setup: &StaticSetup,
    plugins: bool,
    url: &GoUrl,
    path: &str,
    parts: &Parts,
) -> Option<Response> {
    let mut headers = HeaderMap::new();
    let cache_control = if go_path::base(path) == "remote_entry.js" {
        CACHE_REVALIDATE
    } else {
        CACHE_FOREVER
    };
    set_header(&mut headers, "cache-control", cache_control);
    set_header(&mut headers, "permissions-policy", "");
    set_header(&mut headers, "x-content-type-options", "nosniff");
    set_header(&mut headers, "referrer-policy", "no-referrer");

    if path.ends_with('/') {
        return Some(not_found_no_cache(serve_error(
            headers,
            "404 page not found",
            StatusCode::NOT_FOUND,
        )));
    }

    // `http.StripPrefix(path.Join(subpath, "static"[, "plugins"]), …)`: both the path and, when
    // set, the raw path must lose the prefix, or it is a 404.
    let prefix = if plugins {
        go_path::join(&[&setup.subpath, "static", "plugins"])
    } else {
        go_path::join(&[&setup.subpath, "static"])
    };
    let raw_path = String::from_utf8_lossy(&url.raw_path);
    let stripped = path.strip_prefix(prefix.as_str());
    let raw_stripped = raw_path.is_empty() || raw_path.starts_with(prefix.as_str());
    let Some(stripped) = stripped.filter(|_| raw_stripped) else {
        return Some(not_found_no_cache(serve_error(
            headers,
            "404 page not found",
            StatusCode::NOT_FOUND,
        )));
    };

    // `fileHandler.ServeHTTP`.
    let upath = if stripped.starts_with('/') {
        stripped.to_owned()
    } else {
        format!("/{stripped}")
    };
    let name = go_path::clean(&upath);
    let dir = if plugins {
        &setup.plugin_client_dir
    } else {
        &setup.static_dir
    };
    let response = serve_file(dir, &name, &upath, &url.raw_query, true, parts, headers).await?;
    Some(not_found_no_cache(response))
}

/// `notFoundNoCacheResponseWriter.WriteHeader` (static.go:141).
fn not_found_no_cache(mut response: Response) -> Response {
    if response.status() == StatusCode::NOT_FOUND {
        set_header(response.headers_mut(), "cache-control", CACHE_NOT_FOUND);
    }
    response
}

/// Port of `http.serveFile` (net/http/fs.go:679) over `http.Dir(dir)`.
///
/// `name` is the cleaned file name, `url_path` the request's (stripped) path, which is what the
/// redirects look at. `None` forwards: a directory listing, a multi-range request.
pub(crate) async fn serve_file(
    dir: &Path,
    name: &str,
    url_path: &str,
    raw_query: &str,
    redirect: bool,
    parts: &Parts,
    headers: HeaderMap,
) -> Option<Response> {
    if url_path.ends_with("/index.html") {
        return Some(local_redirect(headers, "./", raw_query, &parts.method));
    }

    let full = match dir_open_path(dir, name) {
        Ok(full) => full,
        Err(()) => {
            return Some(serve_error(
                headers,
                "404 page not found",
                StatusCode::NOT_FOUND,
            ));
        }
    };
    let file = match tokio::fs::File::open(&full).await {
        Ok(file) => file,
        Err(err) => {
            let (msg, code) = to_http_error(&map_open_error(err, &full));
            return Some(serve_error(headers, msg, code));
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(err) => {
            let (msg, code) = to_http_error(&err);
            return Some(serve_error(headers, msg, code));
        }
    };

    if redirect {
        if metadata.is_dir() {
            if !url_path.ends_with('/') {
                let target = format!("{}/", go_path::base(url_path));
                return Some(local_redirect(headers, &target, raw_query, &parts.method));
            }
        } else if url_path.ends_with('/') {
            let base = go_path::base(url_path);
            if base == "/" || base == "." {
                return Some(serve_error(
                    headers,
                    "http: attempting to traverse a non-directory",
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
            let target = format!("../{base}");
            return Some(local_redirect(headers, &target, raw_query, &parts.method));
        }
    }
    if metadata.is_dir() {
        // A directory named without its slash — reachable only when `redirect` is off, through
        // `http.ServeFile` (a plugin's public file), since the block above takes it otherwise.
        if !url_path.ends_with('/') {
            let target = format!("{}/", go_path::base(url_path));
            return Some(local_redirect(headers, &target, raw_query, &parts.method));
        }
        // `index.html` or `dirList`. Unreachable through the static handlers, which 404 a
        // trailing slash before the file server sees it, and redirect one that is missing.
        return None;
    }

    let content_type = match sniff_content_type(&full, &file).await {
        Ok(content_type) => content_type,
        Err(()) => {
            return Some(serve_error(
                headers,
                "seeker can't seek",
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };
    let modtime_millis = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());

    match crate::serve_content::serve_content_typed_late(
        headers,
        &content_type,
        modtime_millis,
        &parts.method,
        &parts.headers,
        file,
        metadata.len(),
    )
    .await
    {
        FileResponse::Response(response) => Some(response),
        FileResponse::Forward(reason) => {
            tracing::debug!(reason, "forwarding a static file to Go");
            None
        }
    }
}

/// `http.Dir.Open`'s path arithmetic: `path.Clean("/"+name)[1:]` (or `.`), refused by
/// `filepath.Localize` when it holds a NUL, then joined under the directory. `Err` is
/// `errInvalidUnsafePath`, which `toHTTPError` makes a 404.
fn dir_open_path(dir: &Path, name: &str) -> Result<PathBuf, ()> {
    let cleaned = go_path::clean(&format!("/{name}"));
    let rel = &cleaned[1..];
    let rel = if rel.is_empty() { "." } else { rel };
    if rel.contains('\0') {
        return Err(());
    }
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    Ok(dir.join(rel))
}

/// Port of `mapOpenError` (fs.go:49): an error that is neither not-exist nor permission becomes
/// not-exist when some prefix of the path is a regular file — `ENOTDIR` for `a.js/b` is a 404,
/// not a 500.
fn map_open_error(err: std::io::Error, full: &Path) -> std::io::Error {
    use std::io::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::NotFound | ErrorKind::PermissionDenied
    ) {
        return err;
    }
    let text = full.to_string_lossy();
    let parts: Vec<&str> = text.split('/').collect();
    for i in 0..parts.len() {
        if parts[i].is_empty() {
            continue;
        }
        let prefix = parts[..=i].join("/");
        match std::fs::metadata(&prefix) {
            Err(_) => return err,
            Ok(info) if !info.is_dir() => return std::io::Error::from(ErrorKind::NotFound),
            Ok(_) => {}
        }
    }
    err
}

/// Port of `toHTTPError` (fs.go:769).
fn to_http_error(err: &std::io::Error) -> (&'static str, StatusCode) {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => ("404 page not found", StatusCode::NOT_FOUND),
        ErrorKind::PermissionDenied => ("403 Forbidden", StatusCode::FORBIDDEN),
        _ => (
            "500 Internal Server Error",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// `serveContent`'s type choice: `mime.TypeByExtension(filepath.Ext(name))`, else
/// `DetectContentType` over the first 512 bytes (fs.go:283-297). The file is read through a
/// second handle so the one being served stays at offset 0.
async fn sniff_content_type(full: &Path, _file: &tokio::fs::File) -> Result<String, ()> {
    let base = full
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = filepath_ext(&base);
    let by_extension = mm_app::mime::type_by_extension(ext);
    if !by_extension.is_empty() {
        return Ok(by_extension);
    }
    use tokio::io::AsyncReadExt as _;
    let mut sniffer = tokio::fs::File::open(full).await.map_err(|_| ())?;
    let mut buf = Vec::with_capacity(512);
    (&mut sniffer)
        .take(512)
        .read_to_end(&mut buf)
        .await
        .map_err(|_| ())?;
    Ok(mm_app::link_image::detect_content_type(&buf).to_owned())
}

/// Port of `filepath.Ext`: from the last `.` of the final element, or `""`.
fn filepath_ext(base: &str) -> &str {
    match base.rfind(['.', '/']) {
        Some(i) if base.as_bytes()[i] == b'.' => &base[i..],
        _ => "",
    }
}

/// Port of `localRedirect` (fs.go:785): a relative `Location`, the query carried over, a 301
/// with no body — and whatever headers the handler had already set.
fn local_redirect(headers: HeaderMap, target: &str, raw_query: &str, method: &Method) -> Response {
    let mut headers = headers;
    set_header(&mut headers, "location", &with_query(target, raw_query));
    build_response(StatusCode::MOVED_PERMANENTLY, headers, empty_body(method))
}

/// `root` behind `NewStaticHandler` — the SPA page, `root.html`, for every path nothing else
/// claimed (static.go:61-124 and the `IsStatic` arm of handlers.go:183-367).
async fn root(
    state: &AppState,
    setup: &StaticSetup,
    raw_target: &str,
    path: &str,
    parts: &Parts,
) -> Option<Response> {
    let config = mm_app::config::load_model_config(state.app.store().config())
        .await
        .map_err(|err| tracing::warn!(error = %err, "could not read the configuration"))
        .ok()?;
    let service = &config.service_settings;

    // `basicSecurityChecks`: an over-long request URI is `RenderWebAppError`'s signed page.
    let max_url = service.maximum_url_length.unwrap_or(2048);
    if i64::try_from(raw_target.len()).unwrap_or(i64::MAX) > max_url {
        return None;
    }

    // The session half of `ServeHTTP`. `RequireSession` is false, so a bad token changes nothing
    // — but `GetSession` on a good one still runs, with its side effects, and three outcomes are
    // Go's error page rather than the SPA. The cloud and remote-cluster headers take branches
    // that depend on a licence this port leaves to Go.
    // A query-string token: a valid non-OAuth session there is `token_provided`, an error page
    // this handler still leaves to Go ([D-901]).
    if crate::auth::parse_auth_token(parts)
        .is_some_and(|(_, location)| location == crate::auth::TokenLocation::QueryString)
    {
        return None;
    }
    match session_preamble(state, parts).await {
        Preamble::Continue => {}
        // Every error here is `RenderWebAppError`'s page ([D-901]).
        Preamble::Forward | Preamble::Error(_) => return None,
    }

    let mut headers =
        static_handler_headers(state, setup, &config, &mm_model::utils::new_id()).await?;

    // `root` itself.
    let user_agent = parts
        .headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !check_client_compatibility(user_agent) {
        return None;
    }
    let minimum_desktop = service
        .minimum_desktop_app_version
        .as_deref()
        .unwrap_or_default();
    if !minimum_desktop.is_empty()
        && mm_app::user_agent::get_desktop_app_version(user_agent).is_some()
    {
        return None;
    }

    if is_api_call(path, &setup.subpath) {
        return Some(handle_404(
            state.app.config().default_server_locale.as_str(),
            headers,
            path,
        ));
    }

    set_header(&mut headers, "cache-control", CACHE_REVALIDATE);
    let (static_dir, found) = mm_app::logs::find_dir(CLIENT_DIR);
    let root_html = if found {
        static_dir.join("root.html")
    } else {
        PathBuf::from("root.html")
    };
    let contents = match tokio::fs::read(&root_html).await {
        Ok(contents) => contents,
        Err(err) => {
            tracing::warn!(file_path = %root_html.display(), error = %err, "failed to read root.html");
            return Some(http_error(
                headers,
                &go_read_file_error(&root_html, &err),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };

    let original = format!("<title>{}</title>", html_escape_string(DEFAULT_SITE_NAME));
    let modified = get_open_graph_meta_tags(
        config.team_settings.site_name.as_deref(),
        config.team_settings.custom_description_text.as_deref(),
    );
    let contents = if original == modified {
        contents
    } else {
        replace_all(&contents, original.as_bytes(), modified.as_bytes())
    };

    set_header(&mut headers, "content-type", "text/html");
    Some(build_response(
        StatusCode::OK,
        headers,
        gzhttp::go_framed_body(Bytes::from(contents)),
    ))
}

/// What the session half of `ServeHTTP` (web/handlers.go:268-315) decides for a handler with
/// `RequireSession: false` — the page handler and `/manualtest` — before the handler runs.
pub(crate) enum Preamble {
    /// No token, a token that resolves to no session, or a session the handler may use.
    Continue,
    /// A branch this port leaves to Go: the cloud and remote-cluster token headers (licence-gated
    /// sessions), and a resolved session under `FeatureFlags.SessionAttributes`, whose
    /// `ProcessSessionAttributesRequest` (Enterprise Advanced) is not ported.
    Forward,
    /// `c.Err`: `GetSession`'s 500, or `api.context.token_provided.app_error` for a valid
    /// non-OAuth session presented as `?access_token=`.
    Error(Box<mm_model::utils::AppError>),
}

/// The session half of `ServeHTTP` for a handler that needs no session. A token that does not
/// resolve is not an error — `RequireSession` is false — but `GetSession` on one that does still
/// runs, with its side effects. No CSRF check: it applies only to a non-`GET` request of a
/// session-required handler.
pub(crate) async fn session_preamble(state: &AppState, parts: &Parts) -> Preamble {
    if crate::auth::parse_service_token(parts).is_some() {
        return Preamble::Forward;
    }
    let Some((token, location)) = crate::auth::parse_auth_token(parts) else {
        return Preamble::Continue;
    };
    match state.app.get_session(&token).await {
        Err(err) if err.status_code == 500 => Preamble::Error(err),
        Err(_) => Preamble::Continue,
        Ok(_) if state.app.config().feature_flags.session_attributes => Preamble::Forward,
        Ok(session) if !session.is_oauth && location == crate::auth::TokenLocation::QueryString => {
            Preamble::Error(mm_model::utils::AppError::boxed(
                "ServeHTTP",
                "api.context.token_provided.app_error",
                None,
                format!("token={token}"),
                401,
            ))
        }
        Ok(_) => Preamble::Continue,
    }
}

/// [`serve_http_headers`] plus the `IsStatic` pair `ServeHTTP` adds for `NewStaticHandler`
/// (web/handlers.go:245-257): `X-Frame-Options` and the content security policy.
async fn static_handler_headers(
    state: &AppState,
    setup: &StaticSetup,
    config: &mm_model::config::Config,
    request_id: &str,
) -> Option<HeaderMap> {
    let service = &config.service_settings;
    let mut headers = serve_http_headers(state, config, request_id).await?;
    set_header(&mut headers, "x-frame-options", "SAMEORIGIN");
    set_header(
        &mut headers,
        "content-security-policy",
        &format!(
            "frame-ancestors 'self' {}; script-src 'self'{}{}",
            service.frame_ancestors.as_deref().unwrap_or_default(),
            setup.csp_sha_directive,
            generate_dev_csp(service.developer_flags.as_deref().unwrap_or_default()),
        ),
    );
    Some(headers)
}

/// The headers `ServeHTTP` sets on every response once `basicSecurityChecks` has passed
/// (web/handlers.go:234-244): the request id, the version id — which carries the client-config
/// hash and whether a licence is loaded — `Strict-Transport-Security` when configured, and the
/// three fixed security headers. `None` when the licence or the hash cannot be read.
pub(crate) async fn serve_http_headers(
    state: &AppState,
    config: &mm_model::config::Config,
    request_id: &str,
) -> Option<HeaderMap> {
    let service = &config.service_settings;
    let license = state
        .app
        .license()
        .await
        .map_err(|err| tracing::warn!(error = %err.id, "could not read the licence"))
        .ok()?;
    let hash = state
        .app
        .client_config_hash(config)
        .await
        .map_err(|err| tracing::warn!(error = %err, "could not hash the client config"))
        .ok()?;

    let mut headers = HeaderMap::new();
    set_header(&mut headers, "x-request-id", request_id);
    set_header(
        &mut headers,
        "x-version-id",
        &format!(
            "{}.{}.{}.{}",
            mm_model::version::CURRENT_VERSION,
            mm_model::version::BUILD_NUMBER,
            hash,
            license.is_some()
        ),
    );
    if service.tls_strict_transport.unwrap_or(false) {
        set_header(
            &mut headers,
            "strict-transport-security",
            &format!(
                "max-age={}",
                service.tls_strict_transport_max_age.unwrap_or(63_072_000)
            ),
        );
    }
    set_header(&mut headers, "permissions-policy", "");
    set_header(&mut headers, "x-content-type-options", "nosniff");
    set_header(&mut headers, "referrer-policy", "no-referrer");
    Some(headers)
}

/// `bytes.ReplaceAll`.
fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.windows(from.len()).position(|w| w == from) {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Port of `getOpenGraphMetaTags` (static.go:195): the configured site name as the `<title>`,
/// and an `og:description` only when a description is configured.
fn get_open_graph_meta_tags(site_name: Option<&str>, description: Option<&str>) -> String {
    let site_name = site_name
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SITE_NAME);
    // `model.TeamSettingsDefaultCustomDescriptionText` is `""`, so "unset" and "empty" are the
    // same answer: no `og:description`.
    let description = description.unwrap_or_default();
    let mut out = format!("<title>{}</title>", html_escape_string(site_name));
    if !description.is_empty() {
        out.push_str(&format!(
            "<meta property=\"og:description\" content=\"{}\" />",
            html_escape_string(description)
        ));
    }
    out
}

/// Port of `generateDevCSP` (handlers.go:95): `'unsafe-eval'` and `'unsafe-inline'` for a `dev`
/// build, plus each of the two named in `DeveloperFlags` with the value `true` on any other build.
fn generate_dev_csp(developer_flags: &str) -> String {
    let dev_build = mm_model::version::BUILD_NUMBER == "dev";
    let mut dev_csp: Vec<String> = Vec::new();
    if dev_build {
        dev_csp.push("'unsafe-eval'".to_owned());
        dev_csp.push("'unsafe-inline'".to_owned());
    }
    if !developer_flags.is_empty() {
        for flag in developer_flags.split(',') {
            let Some((key, value)) = flag.split_once('=') else {
                continue;
            };
            if value != "true" {
                continue;
            }
            if (key == "unsafe-eval" || key == "unsafe-inline") && !dev_build {
                dev_csp.push(format!("'{key}'"));
            }
        }
    }
    if dev_csp.is_empty() {
        return String::new();
    }
    format!(" {}", dev_csp.join(" "))
}

/// Port of `CheckClientCompatibility` (web.go:52): only IE and Safari have a floor, 12 for both.
fn check_client_compatibility(user_agent: &str) -> bool {
    use mm_app::user_agent::BrowserName;
    let ua = mm_app::user_agent::parse(user_agent);
    let minimum = match ua.browser_name {
        BrowserName::IE | BrowserName::Safari => 12,
        _ => return true,
    };
    ua.browser_version.major >= minimum
}

/// Port of `IsAPICall` (web.go:100).
fn is_api_call(path: &str, subpath: &str) -> bool {
    path.starts_with(&format!("{}/", go_path::join(&[subpath, "api"])))
}

/// Port of `Handle404` (web.go:79) for an API path: the JSON `AppError`, **with** its detailed
/// error — it is written directly, not through `handleContextError`, so nothing wipes it — and
/// without a request id.
///
/// Its message is the one `NewAppError` set at construction, which is `i18n.T`: the
/// **`DefaultServerLocale`** translation, not the caller's. This page never sees an
/// `Accept-Language`, because `handleContextError` is exactly what it does not go through.
pub(crate) fn handle_404(server_locale: &str, mut headers: HeaderMap, path: &str) -> Response {
    let mut err =
        mm_model::utils::AppError::new("Handle404", "api.context.404.app_error", None, "", 404);
    if let Some(bundle) = mm_app::i18n::loaded() {
        bundle.translate_app_error(bundle.server_locale(server_locale), &mut err);
    }
    err.detailed_error = format!(
        "There doesn't appear to be an api call for the url='{path}'.  Typo? are you missing a team_id or user_id as part of the url?"
    );
    set_header(&mut headers, "content-type", "application/json");
    let body = mm_model::utils::go_json_marshal(&err).unwrap_or_default();
    build_response(StatusCode::NOT_FOUND, headers, Body::from(body))
}

/// `http.Error` (server.go:2301).
fn http_error(mut headers: HeaderMap, text: &str, code: StatusCode) -> Response {
    headers.remove(header::CONTENT_LENGTH);
    set_header(&mut headers, "content-type", "text/plain; charset=utf-8");
    set_header(&mut headers, "x-content-type-options", "nosniff");
    build_response(code, headers, Body::from(format!("{text}\n")))
}

/// `err.Error()` for the `*PathError` `os.ReadFile` returns: `open <path>: <errno>`, or `read`
/// for a directory (the open succeeds and the read fails). Go's errno strings are its own table,
/// which is libc's text with a lower-case first letter.
fn go_read_file_error(path: &Path, err: &std::io::Error) -> String {
    let op = if err.kind() == std::io::ErrorKind::IsADirectory {
        "read"
    } else {
        "open"
    };
    let text = err.to_string();
    let text = text
        .rfind(" (os error ")
        .map_or(text.as_str(), |at| &text[..at]);
    let mut chars = text.chars();
    let lowered = match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    };
    format!("{op} {}: {lowered}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(subpath: &str) -> StaticSetup {
        StaticSetup {
            webserver_mode: "gzip".to_owned(),
            subpath: subpath.to_owned(),
            static_dir: PathBuf::from("./"),
            plugin_client_dir: PathBuf::from("./client/plugins"),
            csp_sha_directive: String::new(),
            enable_testing: false,
            host_plugins: false,
        }
    }

    /// Which requests Go hands to a `web.Handler`, and which kind — every web route by method, the
    /// catch-all for `GET`/`HEAD` (including a web route's path under a method it does not take),
    /// and none for the api4 tree, the plugin subrouter and the three plain handlers.
    #[test]
    fn go_handler_kind_follows_gorillas_match_order() {
        use HandlerKind::{Api, Static};
        let s = setup("/");
        let (get, head, post, put) = (Method::GET, Method::HEAD, Method::POST, Method::PUT);
        for (method, path, want) in [
            (&get, "/", Some(Static)),
            (&head, "/", Some(Static)),
            (&post, "/", None),
            (&get, "/team/channels/town-square", Some(Static)),
            (&get, "/api/v4", Some(Static)),
            (&get, "/api/v5/x", Some(Static)),
            (&get, "/api/v4/users/me", None),
            (&get, "/api/v4/oauth_test", None),
            (&get, "/plugins/com.x/y", None),
            (&post, "/plugins/com.x", None),
            (&get, "/plugins/", Some(Static)),
            (&get, "/static/main.js", None),
            (&get, "/static", Some(Static)),
            (&get, "/robots.txt", None),
            (&get, "/unsupported_browser.js", None),
            (&get, "/login/one_time_link", Some(Api)),
            (&post, "/login/one_time_link", None),
            (&get, "/login/sso/saml", Some(Api)),
            (&post, "/login/sso/saml", Some(Api)),
            (&put, "/login/sso/saml", None),
            (&get, "/oauth/authorize", Some(Api)),
            (&post, "/oauth/authorize", Some(Api)),
            (&post, "/oauth/deauthorize", Some(Api)),
            (&get, "/oauth/deauthorize", Some(Static)),
            (&post, "/oauth/access_token", Some(Api)),
            (&get, "/oauth/access_token", Some(Static)),
            (&post, "/oauth/intune", Some(Api)),
            (&get, "/oauth/gitlab/login", Some(Api)),
            (&get, "/oauth/gitlab/mobile_login", Some(Api)),
            (&get, "/oauth/git-lab/login", Some(Static)),
            (&head, "/oauth/gitlab/login", Some(Static)),
            (&get, "/oauth/gitlab/login/", Some(Static)),
            (&get, "/api/v3/oauth/gitlab/complete", Some(Api)),
            (&get, "/signup/gitlab/complete", Some(Api)),
            (&get, "/login/gitlab/complete", Some(Api)),
            (&post, "/hooks/abc", Some(Api)),
            (&post, "/hooks/commands/abc", Some(Api)),
            (&post, "/hooks/a_b", None),
            (&get, "/hooks/abc", Some(Static)),
            (&get, "/.well-known/oauth-authorization-server", Some(Api)),
            (&get, "/.well-known/oauth-authorization-serverx", Some(Api)),
            (
                &head,
                "/.well-known/oauth-authorization-server",
                Some(Static),
            ),
            (&get, "/manualtest", Some(Static)),
        ] {
            assert_eq!(go_handler_kind(&s, method, path), want, "{method} {path}");
        }
        let testing = StaticSetup {
            enable_testing: true,
            ..setup("/")
        };
        assert_eq!(go_handler_kind(&testing, &get, "/manualtest"), Some(Api));
        assert_eq!(
            go_handler_kind(&testing, &head, "/manualtest"),
            Some(Static)
        );
        let disabled = StaticSetup {
            webserver_mode: "disabled".to_owned(),
            ..setup("/")
        };
        assert_eq!(go_handler_kind(&disabled, &get, "/"), None);
        assert_eq!(
            go_handler_kind(&disabled, &get, "/oauth/gitlab/login"),
            Some(Api)
        );
        let sub = setup("/mm");
        assert_eq!(go_handler_kind(&sub, &get, "/mm/"), Some(Static));
        assert_eq!(
            go_handler_kind(&sub, &get, "/mm/oauth/authorize"),
            Some(Api)
        );
        assert_eq!(
            go_handler_kind(&sub, &get, "/oauth/authorize"),
            None,
            "the redirect"
        );
        assert_eq!(go_handler_kind(&setup(""), &get, "/"), None);
    }

    #[test]
    fn gorillas_clean_path_keeps_a_trailing_slash_and_roots_the_path() {
        assert_eq!(mux_clean_path(""), "/");
        assert_eq!(mux_clean_path("/static/../x"), "/x");
        assert_eq!(mux_clean_path("/static/images/"), "/static/images/");
        assert_eq!(mux_clean_path("/a//b/./c/"), "/a/b/c/");
        assert_eq!(mux_clean_path("/"), "/");
        assert_eq!(mux_clean_path("/.."), "/");
    }

    /// The plugin subrouter is served only when this process hosts the plugins, for every
    /// method and under the subpath; a path gorilla would not match is forwarded as before.
    #[test]
    fn the_plugin_routes_are_served_only_under_the_rust_host() {
        use crate::plugin_requests::PluginRoute;
        let go_host = setup("/");
        assert_eq!(
            classify(&go_host, &Method::GET, "/plugins/p/x", ""),
            Route::Forward
        );
        let mut rust_host = setup("/");
        rust_host.host_plugins = true;
        assert_eq!(
            classify(&rust_host, &Method::POST, "/plugins/p/x", ""),
            Route::Plugin(PluginRoute::Request("p".to_owned()))
        );
        assert_eq!(
            classify(&rust_host, &Method::GET, "/plugins/p/public/a.png", ""),
            Route::Plugin(PluginRoute::Public("p".to_owned()))
        );
        assert_eq!(
            classify(&rust_host, &Method::GET, "/plugins/a~b/x", ""),
            Route::Forward
        );
        let mut sub = setup("/chat");
        sub.host_plugins = true;
        assert_eq!(
            classify(&sub, &Method::DELETE, "/chat/plugins/p", ""),
            Route::Plugin(PluginRoute::Request("p".to_owned()))
        );
        assert_eq!(
            classify(&sub, &Method::GET, "/plugins/p", ""),
            Route::Redirect("/chat/plugins/p".to_owned()),
            "outside the subpath is still the redirect into it"
        );
    }

    #[test]
    fn classify_sends_only_the_static_routes_and_the_catch_all_here() {
        let s = setup("/");
        let get = Method::GET;
        assert_eq!(classify(&s, &get, "/", ""), Route::Root);
        assert_eq!(classify(&s, &get, "/login", ""), Route::Root);
        assert_eq!(
            classify(&s, &get, "/team/channels/town-square", ""),
            Route::Root
        );
        assert_eq!(
            classify(&s, &get, "/static/main.js", ""),
            Route::Static { plugins: false }
        );
        assert_eq!(
            classify(&s, &get, "/static/plugins/p/x.js", ""),
            Route::Static { plugins: true }
        );
        assert_eq!(classify(&s, &get, "/static", ""), Route::Root);
        assert_eq!(classify(&s, &get, "/robots.txt", ""), Route::Robots);
        assert_eq!(classify(&s, &get, "/robots.txt/", ""), Route::Root);
        assert_eq!(
            classify(&s, &get, "/unsupported_browser.js", ""),
            Route::UnsupportedBrowserScript
        );
        // The API 404 is root's — except under the api4 tree, which Go's api4 answers.
        assert_eq!(classify(&s, &get, "/api/v3/x", ""), Route::Root);
        assert_eq!(classify(&s, &get, "/api/v4/nope", ""), Route::Forward);
        assert_eq!(classify(&s, &get, "/api/v4", ""), Route::Forward);
        assert_eq!(classify(&s, &get, "/plugins/x/y", ""), Route::Forward);
        assert_eq!(classify(&s, &get, "/login/sso/saml", ""), Route::Forward);
        assert_eq!(
            classify(&s, &get, "/login/gitlab/complete", ""),
            Route::Forward
        );
        assert_eq!(classify(&s, &get, "/login/desktop", ""), Route::Root);
        assert_eq!(classify(&s, &get, "/manualtest", ""), Route::Root);
        assert_eq!(classify(&s, &Method::POST, "/login", ""), Route::Forward);
        assert_eq!(classify(&s, &Method::HEAD, "/", ""), Route::Root);
        let testing = StaticSetup {
            enable_testing: true,
            ..setup("/")
        };
        assert_eq!(
            classify(&testing, &get, "/manualtest", ""),
            Route::ManualTest
        );
        assert_eq!(
            classify(&testing, &Method::HEAD, "/manualtest", ""),
            Route::Root
        );
        assert_eq!(classify(&testing, &get, "/manualtest/", ""), Route::Root);
        assert_eq!(
            classify(&testing, &Method::POST, "/manualtest", ""),
            Route::Forward
        );
        let testing_sub = StaticSetup {
            enable_testing: true,
            ..setup("/chat")
        };
        assert_eq!(
            classify(&testing_sub, &get, "/chat/manualtest", ""),
            Route::ManualTest
        );
    }

    #[test]
    fn a_subpath_redirects_its_bare_self_and_everything_outside_it() {
        let s = setup("/chat");
        let get = Method::GET;
        assert_eq!(
            classify(&s, &get, "/chat", "a=1"),
            Route::Redirect("/chat/?a=1".to_owned())
        );
        assert_eq!(
            classify(&s, &get, "/chatroom", ""),
            Route::Redirect("/chat/chatroom".to_owned())
        );
        assert_eq!(
            classify(&s, &get, "/static/x.js", ""),
            Route::Redirect("/chat/static/x.js".to_owned())
        );
        assert_eq!(
            classify(&s, &get, "/chat/static/x.js", ""),
            Route::Static { plugins: false }
        );
        assert_eq!(classify(&s, &get, "/chat/", ""), Route::Root);
        assert_eq!(classify(&setup(""), &get, "/", ""), Route::Forward);
    }

    #[test]
    fn the_api_404_prefix_follows_the_subpath() {
        assert!(is_api_call("/api/v5/x", "/"));
        assert!(!is_api_call("/apix", "/"));
        assert!(!is_api_call("/api", "/"));
        assert!(is_api_call("/chat/api/x", "/chat"));
        assert!(!is_api_call("/api/x", "/chat"));
    }

    #[test]
    fn open_graph_tags_replace_the_title_and_add_a_description_only_when_set() {
        assert_eq!(
            get_open_graph_meta_tags(None, None),
            "<title>Mattermost</title>"
        );
        assert_eq!(
            get_open_graph_meta_tags(Some(""), Some("")),
            "<title>Mattermost</title>"
        );
        assert_eq!(
            get_open_graph_meta_tags(Some("R&D <chat>"), Some("It's \"ours\"")),
            "<title>R&amp;D &lt;chat&gt;</title><meta property=\"og:description\" content=\"It&#39;s &#34;ours&#34;\" />"
        );
    }

    #[test]
    fn dev_csp_honours_only_the_two_known_flags_set_to_true() {
        assert_eq!(generate_dev_csp(""), "");
        assert_eq!(
            generate_dev_csp("unsafe-eval=true,unsafe-inline=true"),
            " 'unsafe-eval' 'unsafe-inline'"
        );
        assert_eq!(generate_dev_csp("unsafe-eval=false,unsafe-inline"), "");
        assert_eq!(
            generate_dev_csp("other=true,unsafe-inline=true"),
            " 'unsafe-inline'"
        );
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        assert_eq!(replace_all(b"a<t>b<t>", b"<t>", b"X"), b"aXbX".to_vec());
        assert_eq!(replace_all(b"none", b"<t>", b"X"), b"none".to_vec());
    }

    #[test]
    fn filepath_ext_is_from_the_last_dot_of_the_last_element() {
        assert_eq!(filepath_ext("main.abc.js"), ".js");
        assert_eq!(filepath_ext("LICENSE"), "");
        assert_eq!(filepath_ext(".hidden"), ".hidden");
    }

    #[test]
    fn dir_open_refuses_a_nul_and_cannot_climb_out() {
        let dir = Path::new("/srv/client");
        assert_eq!(
            dir_open_path(dir, "/../../etc/passwd"),
            Ok(PathBuf::from("/srv/client/etc/passwd"))
        );
        assert_eq!(dir_open_path(dir, "/"), Ok(PathBuf::from("/srv/client/.")));
        assert_eq!(dir_open_path(dir, "/a\0b"), Err(()));
    }

    #[test]
    fn the_read_error_text_is_gos() {
        let err = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            go_read_file_error(Path::new("/x/client/root.html"), &err),
            "open /x/client/root.html: no such file or directory"
        );
        let err = std::io::Error::from_raw_os_error(21);
        assert_eq!(
            go_read_file_error(Path::new("/x/root.html"), &err),
            "read /x/root.html: is a directory"
        );
    }

    #[test]
    fn the_safari_and_ie_floor_is_twelve() {
        // Safari 11 on macOS, Safari 17, IE 11 through Trident, Edge (IE family, version 120).
        assert!(!check_client_compatibility(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_13_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/11.1.2 Safari/605.1.15"
        ));
        assert!(check_client_compatibility(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"
        ));
        assert!(!check_client_compatibility(
            "Mozilla/5.0 (Windows NT 10.0; WOW64; Trident/7.0; rv:11.0) like Gecko"
        ));
        assert!(check_client_compatibility(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0"
        ));
        assert!(check_client_compatibility(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"
        ));
        assert!(check_client_compatibility(""));
    }

    #[test]
    fn static_script_hashes_are_empty_at_the_root_and_hash_the_inline_script_otherwise() {
        assert_eq!(get_static_script_hashes("/", false), "");
        assert_eq!(get_static_script_hashes("", false), "");
        assert_eq!(
            get_subpath_script("/chat"),
            "window.publicPath='/chat/static/'"
        );
        let both = get_static_script_hashes("/chat", true);
        assert_eq!(both.matches("'sha256-").count(), 2);
    }
}

#[cfg(test)]
mod go_parity {
    //! Against `fixtures/behaviour_web_static.json` — reference/dump/behaviour_web_static.go.
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_web_static.json"))
            .expect("behaviour_web_static.json is generated by reference/dump")
    }

    #[test]
    fn client_compatibility_matches_go_for_every_agent() {
        let oracle = oracle();
        let rows = oracle["client_compatibility"].as_array().unwrap();
        assert!(
            rows.iter().any(|r| r["compatible"] == false),
            "the corpus has refusals"
        );
        for row in rows {
            let ua = row["user_agent"].as_str().unwrap();
            assert_eq!(
                check_client_compatibility(ua),
                row["compatible"].as_bool().unwrap(),
                "{ua}"
            );
        }
    }

    /// `GetStaticScriptHashes(subpath, cfg.FeatureFlags.EnableConcurrentReact)`
    /// (web/handlers.go:58): the flag, read from the running config (`load_model_config`), picks
    /// which loader script's hash the CSP carries.
    #[test]
    fn the_csp_hash_follows_the_concurrent_react_flag() {
        let setup = |on: bool| {
            let mut flags = mm_model::feature_flags::FeatureFlags::default();
            flags.set_defaults();
            flags.enable_concurrent_react = on;
            let config: mm_model::config::Config = serde_json::from_value(serde_json::json!({
                "FeatureFlags": serde_json::to_value(&flags).unwrap(),
            }))
            .unwrap();
            StaticSetup::from_config(&config, "/".to_owned()).csp_sha_directive
        };
        assert_eq!(setup(true), get_static_script_hashes("/", true));
        assert_eq!(setup(false), get_static_script_hashes("/", false));
        assert_ne!(setup(true), setup(false));
    }

    #[test]
    fn static_script_hashes_match_go() {
        let oracle = oracle();
        for row in oracle["static_script_hashes"].as_array().unwrap() {
            assert_eq!(
                get_static_script_hashes(
                    row["subpath"].as_str().unwrap(),
                    row["enable_concurrent_react"].as_bool().unwrap()
                ),
                row["directive"].as_str().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn an_empty_head_answer_has_no_length_and_a_sized_one_keeps_it() {
        use axum::body::HttpBody as _;
        let oracle = oracle();
        let empty_head = oracle["framing"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["method"] == "HEAD" && r["body_length"] == 0)
            .expect("the corpus has an empty HEAD");
        assert_eq!(empty_head["content_length"], "");

        let empty = head_framing(&Method::HEAD, Response::new(Body::empty()));
        assert_eq!(empty.body().size_hint().exact(), None);
        let sized = head_framing(&Method::HEAD, Response::new(Body::from("abc")));
        assert_eq!(sized.body().size_hint().exact(), Some(3));
        let get = head_framing(&Method::GET, Response::new(Body::empty()));
        assert_eq!(get.body().size_hint().exact(), Some(0));
    }
}
