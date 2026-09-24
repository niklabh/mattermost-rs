//! The rate limiter: port of `app.RateLimiter` (channels/app/ratelimit.go) over the GCRA and the
//! in-memory store it is built on (github.com/throttled/throttled/v2 `rate.go`,
//! `store/memstore`).
//!
//! # Where Go applies it — three places, all ported
//!
//! 1. **Per route** — `api.RateLimitedHandler` (api4/handlers.go:222) wraps three registrations,
//!    each with its own limiter and its own store: `POST /users/login` at 5/s with a burst of 10
//!    (api4/user.go:69), and `POST /users/login/desktop_token` and `POST /oauth/apps/register` at
//!    2/s with a burst of 1 (user.go:71, oauth.go:24). Only the rate and burst are given;
//!    `SetDefaults` supplies the rest, so each keys on the **peer address alone** —
//!    `VaryByRemoteAddr` true, `VaryByUser` false, no header, and `NewRateLimiter(&settings,
//!    []string{})`: no trusted proxy header, whatever the configuration says. [`route`].
//! 2. **Globally** — `Server.Start` wraps the whole root router in `RateLimitHandler` with the
//!    configured `RateLimitSettings` and `TrustedProxyIPHeader` (app/server.go:1056), outside
//!    everything else: static files, the websocket upgrade, the api4 catch-all. [`global`].
//! 3. **Per user** — `web.Handler.ServeHTTP` (web/handlers.go:288) rate-limits by the session's
//!    user id on the global limiter's store when `VaryByUser` is on, for every request that
//!    carries a token, after the session lookup and before the CSRF check. [`per_user`].
//!
//! All three exist only when `RateLimitSettings.Enable` was true **when the process started**:
//! the route limiters are decided at registration, the global one at `Start`. [`RateLimits`] is
//! read once, on the first request, from the configuration Go was started on.
//!
//! # What the answers look like
//!
//! `RateLimitWriter` adds `X-RateLimit-Limit`, `-Remaining`, `-Reset` (seconds, rounded **up**)
//! and, only when refusing, `Retry-After` — with `Header().Add`, so the global set comes first
//! and a route's or a user's set follows it on the same response. A refusal is
//! `http.Error(w, "limit exceeded", 429)`: `text/plain; charset=utf-8`, `nosniff`, and
//! `limit exceeded\n`. The global and route refusals are written outside every API handler, so
//! they carry nothing else; the per-user refusal is written inside `ServeHTTP`, after its
//! security headers.
//!
//! # Two processes, two stores
//!
//! A forwarded request is limited here and then again by the Go process, whose global limiter
//! sees every forwarded request coming from this server's address. [D-1150] has the details; the
//! global set Go adds to a forwarded answer is replaced by ours, so the headers a client reads are
//! this server's.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::auth::{self, TokenLocation};

/// `time.Second` in nanoseconds.
const SECOND_NS: i64 = 1_000_000_000;

/// Errors building a limiter — `NewRateLimiter`'s two `errors.Wrap`s.
#[derive(Debug, thiserror::Error)]
pub enum RateLimitError {
    /// `throttled.NewGCRARateLimiterCtx`: a negative burst, or a rate that is not positive.
    /// (`PerSec(0)` is an integer division by zero in Go — a panic — and refused here.)
    #[error("invalid RateQuota: per_sec {per_sec}, max_burst {max_burst}")]
    InvalidQuota { per_sec: i64, max_burst: i64 },
}

/// Port of `throttled.RateLimitResult`, durations in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitResult {
    pub limit: i64,
    pub remaining: i64,
    /// `ResetAfter`.
    pub reset_after: i64,
    /// `RetryAfter`: `-1` unless the request was refused.
    pub retry_after: i64,
}

/// Port of `memstore.MemStore` with `maxKeys > 0`: an LRU of `max_keys` keys (hashicorp's
/// `simplelru`) — or, for `max_keys <= 0`, an unbounded map. Every read refreshes a key's
/// recency, a refused request's included (`GetWithTime` goes through `lru.Get`); a new key beyond
/// the capacity evicts the least recently used one, which forgets its history.
#[derive(Debug, Default)]
struct MemStore {
    max_keys: usize,
    /// Key → (TAT in ns, recency stamp).
    entries: HashMap<String, (i64, u64)>,
    /// Recency stamp → key, oldest first. Unused when unbounded.
    order: BTreeMap<u64, String>,
    clock: u64,
}

impl MemStore {
    fn new(max_keys: i64) -> Self {
        Self {
            max_keys: usize::try_from(max_keys).unwrap_or(0),
            ..Self::default()
        }
    }

    fn touch(&mut self, key: &str) {
        if self.max_keys == 0 {
            return;
        }
        self.clock += 1;
        let stamp = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            self.order.remove(&entry.1);
            entry.1 = stamp;
            self.order.insert(stamp, key.to_owned());
        }
    }

    /// `GetWithTime`'s read: the stored TAT, refreshing the key.
    fn get(&mut self, key: &str) -> Option<i64> {
        self.touch(key);
        self.entries.get(key).map(|(tat, _)| *tat)
    }

    /// `SetIfNotExistsWithTTL` / `CompareAndSwapWithTTL` for a caller that holds the lock: the
    /// value is written, the key refreshed, and a new key beyond capacity evicts the oldest.
    fn set(&mut self, key: &str, tat: i64) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.0 = tat;
            self.touch(key);
            return;
        }
        self.clock += 1;
        self.entries.insert(key.to_owned(), (tat, self.clock));
        if self.max_keys == 0 {
            return;
        }
        self.order.insert(self.clock, key.to_owned());
        if self.entries.len() > self.max_keys
            && let Some((_, oldest)) = self.order.pop_first()
        {
            self.entries.remove(&oldest);
        }
    }
}

/// Port of `throttled.GCRARateLimiterCtx` over a [`MemStore`].
///
/// The store's lock is held across the read and the write, which is the serialisation Go's
/// compare-and-swap retry loop converges on; with one process and an in-memory store, Go's
/// `maxCASAttempts` failure cannot happen here and has no counterpart.
#[derive(Debug)]
pub struct Gcra {
    limit: i64,
    delay_variation_tolerance: i64,
    emission_interval: i64,
    store: Mutex<MemStore>,
}

impl Gcra {
    /// `NewGCRARateLimiterCtx(memstore.New(max_keys), RateQuota{PerSec(per_sec), max_burst})`.
    ///
    /// `PerSec(n)` is `time.Second / time.Duration(n)`, an **integer** division: 3/s is a period
    /// of 333,333,333ns, not a third of a second.
    pub fn new(per_sec: i64, max_burst: i64, max_keys: i64) -> Result<Self, RateLimitError> {
        let invalid = RateLimitError::InvalidQuota { per_sec, max_burst };
        if max_burst < 0 || per_sec <= 0 {
            return Err(invalid);
        }
        let period = SECOND_NS / per_sec;
        if period <= 0 {
            return Err(invalid);
        }
        Ok(Self {
            delay_variation_tolerance: period.saturating_mul(max_burst.saturating_add(1)),
            emission_interval: period,
            limit: max_burst.saturating_add(1),
            store: Mutex::new(MemStore::new(max_keys)),
        })
    }

    /// `RateLimitCtx(ctx, key, 1)` at `now` (nanoseconds on any monotonic scale).
    pub fn rate_limit(&self, key: &str, now: i64) -> (bool, RateLimitResult) {
        let mut result = RateLimitResult {
            limit: self.limit,
            remaining: 0,
            reset_after: 0,
            retry_after: -1,
        };
        // A poisoned lock means a panic mid-update of plain integers; the map is still usable.
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let stored = store.get(key);
        // `tat` is the theoretical arrival time of equally spaced requests.
        let tat = stored.unwrap_or(now);
        let increment = self.emission_interval;
        let new_tat = if now > tat {
            now + increment
        } else {
            tat + increment
        };

        let allow_at = new_tat - self.delay_variation_tolerance;
        let diff = now - allow_at;
        let mut limited = false;
        let ttl;
        if diff < 0 {
            ttl = if increment <= self.delay_variation_tolerance {
                result.retry_after = -diff;
                tat - now
            } else {
                0
            };
            limited = true;
        } else {
            ttl = new_tat - now;
            store.set(key, new_tat);
        }

        let next = self.delay_variation_tolerance - ttl;
        if next > -self.emission_interval {
            // Go's `int(next / g.emissionInterval)`: truncation toward zero.
            result.remaining = next / self.emission_interval;
        }
        result.reset_after = ttl;
        (limited, result)
    }
}

/// `model.RateLimitSettings` after `SetDefaults` (config.go:2324).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub per_sec: i64,
    pub max_burst: i64,
    pub memory_store_size: i64,
    pub vary_by_remote_addr: bool,
    pub vary_by_user: bool,
    pub vary_by_header: String,
}

impl Settings {
    /// `SetDefaults` over the document's section.
    pub fn from_model(settings: &mm_model::config::RateLimitSettings) -> Self {
        Self {
            per_sec: settings.per_sec.unwrap_or(10),
            max_burst: settings.max_burst.unwrap_or(100),
            memory_store_size: settings.memory_store_size.unwrap_or(10_000),
            vary_by_remote_addr: settings.vary_by_remote_addr.unwrap_or(true),
            vary_by_user: settings.vary_by_user.unwrap_or(false),
            vary_by_header: settings.vary_by_header.clone(),
        }
    }

    /// A route's `model.RateLimitSettings{PerSec, MaxBurst}` after `SetDefaults`.
    fn route(per_sec: i64, max_burst: i64) -> Self {
        Self::from_model(&mm_model::config::RateLimitSettings {
            per_sec: Some(per_sec),
            max_burst: Some(max_burst),
            ..Default::default()
        })
    }
}

/// What [`RateLimiter::rate_limit_writer`] decided, and the headers `setRateLimitHeaders` adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub limited: bool,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

/// Port of `app.RateLimiter`.
#[derive(Debug)]
pub struct RateLimiter {
    gcra: Gcra,
    use_auth: bool,
    use_ip: bool,
    header: String,
    trusted_proxy_ip_header: Vec<String>,
    /// The zero of the nanosecond scale [`Gcra`] runs on.
    epoch: Instant,
}

impl RateLimiter {
    /// Port of `NewRateLimiter` (ratelimit.go:31).
    pub fn new(
        settings: &Settings,
        trusted_proxy_ip_header: Vec<String>,
    ) -> Result<Self, RateLimitError> {
        Ok(Self {
            gcra: Gcra::new(
                settings.per_sec,
                settings.max_burst,
                settings.memory_store_size,
            )?,
            use_auth: settings.vary_by_user,
            use_ip: settings.vary_by_remote_addr,
            header: settings.vary_by_header.clone(),
            trusted_proxy_ip_header,
            epoch: Instant::now(),
        })
    }

    /// Port of `GenerateKey` (ratelimit.go:60).
    ///
    /// With `VaryByUser`, the token — as `ParseAuthTokenFromRequest` finds it, in any of its six
    /// locations and truncated to 50 bytes — **is** the key, even an empty cookie's; only when no
    /// token was found at all does it fall back to the address, and then only with
    /// `VaryByRemoteAddr`. `VaryByHeader`'s value, lower-cased, is appended in every case.
    pub fn generate_key(&self, parts: &Parts, peer: Option<SocketAddr>) -> String {
        let mut key = String::new();
        let ip = || {
            crate::client_ip::get_ip_address(&parts.headers, peer, &self.trusted_proxy_ip_header)
        };
        if self.use_auth {
            if let Some((token, _)) = auth::parse_auth_token(parts) {
                key.push_str(&token);
            } else if let Some((token, _)) = auth::parse_service_token(parts) {
                key.push_str(&token);
            } else if self.use_ip {
                key.push_str(&ip());
            }
        } else if self.use_ip {
            key.push_str(&ip());
        }
        if !self.header.is_empty() {
            // `r.Header.Get(rl.header)`: the first line, any bytes; `strings.ToLower` is Unicode.
            let value = parts
                .headers
                .get(self.header.as_str())
                .map(|v| String::from_utf8_lossy(v.as_bytes()).to_lowercase())
                .unwrap_or_default();
            key.push_str(&value);
        }
        key
    }

    /// `RateLimitWriter`'s decision and headers, at `now` on the limiter's scale.
    fn verdict_at(&self, key: &str, now: i64) -> Verdict {
        let (limited, result) = self.gcra.rate_limit(key, now);
        Verdict {
            limited,
            headers: rate_limit_headers(&result),
        }
    }

    /// Port of `RateLimitWriter` (ratelimit.go:85), minus the writing: see [`Verdict`] and
    /// [`limit_exceeded`].
    pub fn rate_limit_writer(&self, key: &str) -> Verdict {
        let now = i64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(i64::MAX);
        self.verdict_at(key, now)
    }

    /// Port of `UserIdRateLimit` (ratelimit.go:101): nothing unless `VaryByUser`.
    pub fn user_id_rate_limit(&self, user_id: &str) -> Option<Verdict> {
        self.use_auth.then(|| self.rate_limit_writer(user_id))
    }
}

/// `time.Duration.Seconds()` rounded up, as `setRateLimitHeaders` computes it:
/// `float64(d / Second) + float64(d % Second) / 1e9`, then `math.Ceil`.
fn ceil_seconds(ns: i64) -> i64 {
    let whole = (ns / SECOND_NS) as f64;
    let frac = (ns % SECOND_NS) as f64 / 1e9;
    (whole + frac).ceil() as i64
}

/// Port of `setRateLimitHeaders` (ratelimit.go:117), each guarded on its value being `>= 0`.
fn rate_limit_headers(result: &RateLimitResult) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::with_capacity(4);
    if result.limit >= 0 {
        out.push((X_RATELIMIT_LIMIT, HeaderValue::from(result.limit)));
    }
    if result.remaining >= 0 {
        out.push((X_RATELIMIT_REMAINING, HeaderValue::from(result.remaining)));
    }
    if result.reset_after >= 0 {
        out.push((
            X_RATELIMIT_RESET,
            HeaderValue::from(ceil_seconds(result.reset_after)),
        ));
    }
    if result.retry_after >= 0 {
        out.push((
            header::RETRY_AFTER,
            HeaderValue::from(ceil_seconds(result.retry_after)),
        ));
    }
    out
}

const X_RATELIMIT_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
const X_RATELIMIT_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
const X_RATELIMIT_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");

/// `Header().Add` for each of `headers`.
fn add_headers(target: &mut HeaderMap, headers: &[(HeaderName, HeaderValue)]) {
    for (name, value) in headers {
        target.append(name.clone(), value.clone());
    }
}

/// The refusal: `http.Error(w, "limit exceeded", http.StatusTooManyRequests)` after the headers.
fn limit_exceeded(headers: &[(HeaderName, HeaderValue)]) -> Response {
    let mut response = Response::new(Body::from("limit exceeded\n"));
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    let map = response.headers_mut();
    add_headers(map, headers);
    map.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    map.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    map.insert(crate::error::SERVED_BY, HeaderValue::from_static("rust"));
    response
}

/// A refusal written **outside** every API handler — the global and the route limiters — so
/// `go_global_headers` must leave it as it is.
fn bare_limit_exceeded(headers: &[(HeaderName, HeaderValue)]) -> Response {
    let mut response = limit_exceeded(headers);
    response
        .extensions_mut()
        .insert(crate::web_static::WebOwnHeaders);
    response
}

/// The three routes `RateLimitedHandler` wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `POST /users/login`, 5/s, burst 10.
    Login,
    /// `POST /users/login/desktop_token`, 2/s, burst 1.
    DesktopToken,
    /// `POST /oauth/apps/register`, 2/s, burst 1.
    OAuthRegister,
}

/// Every limiter Go would have built at start, or none of them.
#[derive(Debug, Default)]
pub struct RateLimits {
    pub global: Option<RateLimiter>,
    login: Option<RateLimiter>,
    desktop_token: Option<RateLimiter>,
    oauth_register: Option<RateLimiter>,
}

impl RateLimits {
    /// Build them from the configuration document and `TrustedProxyIPHeader`, as `api4.Init` and
    /// `Server.Start` do: nothing at all unless `Enable`.
    ///
    /// A limiter `NewRateLimiter` refuses is absent: Go's global refusal stops the server
    /// starting, and its route refusal registers a nil handler. Neither can happen on a
    /// configuration `IsValid` accepted, which is every one Go will start on.
    pub fn from_config(
        settings: &mm_model::config::RateLimitSettings,
        trusted_proxy_ip_header: &[String],
    ) -> Self {
        if settings.enable != Some(true) {
            return Self::default();
        }
        let build = |settings: &Settings, trusted: Vec<String>| {
            RateLimiter::new(settings, trusted)
                .map_err(|err| tracing::error!(error = %err, "could not build a rate limiter"))
                .ok()
        };
        Self {
            global: build(
                &Settings::from_model(settings),
                trusted_proxy_ip_header.to_vec(),
            ),
            login: build(&Settings::route(5, 10), Vec::new()),
            desktop_token: build(&Settings::route(2, 1), Vec::new()),
            oauth_register: build(&Settings::route(2, 1), Vec::new()),
        }
    }

    fn route(&self, route: Route) -> Option<&RateLimiter> {
        match route {
            Route::Login => self.login.as_ref(),
            Route::DesktopToken => self.desktop_token.as_ref(),
            Route::OAuthRegister => self.oauth_register.as_ref(),
        }
    }
}

/// The limiters, read from the configuration on the first request and kept for the life of the
/// process — Go's start-time read.
async fn limits(state: &AppState) -> &RateLimits {
    state
        .rate_limits
        .get_or_init(|| async {
            match mm_app::config::load_model_config(state.app.store().config()).await {
                Ok(config) => RateLimits::from_config(
                    &config.rate_limit_settings,
                    config
                        .service_settings
                        .trusted_proxy_ip_header
                        .as_deref()
                        .unwrap_or_default(),
                ),
                Err(err) => {
                    tracing::error!(error = %err, "could not read RateLimitSettings; no rate limiting");
                    RateLimits::default()
                }
            }
        })
        .await
}

fn peer(parts: &Parts) -> Option<SocketAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
}

/// `RateLimitHandler` around the whole TCP router (app/server.go:1056). Outermost, so it sees
/// every request — served, forwarded or the web client's — before anything else runs.
pub(crate) async fn global(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = limits(&state).await.global.as_ref() else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let key = limiter.generate_key(&parts, peer(&parts));
    let verdict = limiter.rate_limit_writer(&key);
    if verdict.limited {
        return bare_limit_exceeded(&verdict.headers);
    }
    let mut response = next.run(Request::from_parts(parts, body)).await;
    let forwarded = response
        .headers()
        .get(crate::error::SERVED_BY)
        .is_some_and(|v| v.as_bytes() == b"go");
    prepend(response.headers_mut(), &verdict.headers, forwarded);
    response
}

/// Put the global set **first**, as Go's outermost `Header().Add` does. On a forwarded answer the
/// first value of each is Go's own global limiter's — keyed on this server's address — and is
/// replaced rather than kept.
fn prepend(headers: &mut HeaderMap, ours: &[(HeaderName, HeaderValue)], forwarded: bool) {
    for (name, value) in ours {
        let mut existing: Vec<HeaderValue> = headers.get_all(name).iter().cloned().collect();
        if forwarded && !existing.is_empty() {
            existing.remove(0);
        }
        headers.remove(name);
        headers.append(name.clone(), value.clone());
        for rest in existing {
            headers.append(name.clone(), rest);
        }
    }
}

/// `RateLimitedHandler` for one route: a `route_layer` on its `POST`.
async fn route(state: AppState, which: Route, request: Request, next: Next) -> Response {
    let Some(limiter) = limits(&state).await.route(which) else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let key = limiter.generate_key(&parts, peer(&parts));
    let verdict = limiter.rate_limit_writer(&key);
    if verdict.limited {
        return bare_limit_exceeded(&verdict.headers);
    }
    let mut response = next.run(Request::from_parts(parts, body)).await;
    add_headers(response.headers_mut(), &verdict.headers);
    response
}

/// [`route`] for `POST /users/login`.
pub(crate) async fn login(State(state): State<AppState>, request: Request, next: Next) -> Response {
    route(state, Route::Login, request, next).await
}

/// [`route`] for `POST /users/login/desktop_token`.
pub(crate) async fn desktop_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    route(state, Route::DesktopToken, request, next).await
}

/// [`route`] for `POST /oauth/apps/register`.
pub(crate) async fn oauth_register(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    route(state, Route::OAuthRegister, request, next).await
}

/// `UserIdRateLimit` in `web.Handler.ServeHTTP` (web/handlers.go:288), as a `route_layer` over
/// every route this server serves — each is a `web.Handler` in Go — and not over the fallbacks,
/// which are Go's to answer.
///
/// It runs whenever a token was found outside the two service headers, **whether or not the
/// session resolved**: a failed lookup or a refused query-string token leaves the context's
/// session empty, so those requests share the key `""`. The refusal is written after `ServeHTTP`'s
/// own headers, which `go_global_headers` adds to it as to any API answer.
pub(crate) async fn per_user(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = limits(&state).await.global.as_ref() else {
        return next.run(request).await;
    };
    if !limiter.use_auth {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Some((token, location)) = auth::parse_auth_token(&parts) else {
        return next.run(Request::from_parts(parts, body)).await;
    };
    if token.is_empty() {
        return next.run(Request::from_parts(parts, body)).await;
    }
    let user_id = match state.app.get_session(&token).await {
        Ok(session) if session.is_oauth || location != TokenLocation::QueryString => {
            session.user_id
        }
        _ => String::new(),
    };
    let Some(verdict) = limiter.user_id_rate_limit(&user_id) else {
        return next.run(Request::from_parts(parts, body)).await;
    };
    if verdict.limited {
        return limit_exceeded(&verdict.headers);
    }
    let mut response = next.run(Request::from_parts(parts, body)).await;
    add_headers(response.headers_mut(), &verdict.headers);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(target: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().method("POST").uri(target);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("a request").into_parts().0
    }

    #[test]
    fn settings_default_as_set_defaults_does() {
        let s = Settings::from_model(&mm_model::config::RateLimitSettings::default());
        assert_eq!(
            s,
            Settings {
                per_sec: 10,
                max_burst: 100,
                memory_store_size: 10_000,
                vary_by_remote_addr: true,
                vary_by_user: false,
                vary_by_header: String::new(),
            }
        );
        let route = Settings::route(5, 10);
        assert_eq!((route.per_sec, route.max_burst), (5, 10));
        assert!(route.vary_by_remote_addr && !route.vary_by_user);
    }

    #[test]
    fn nothing_is_built_unless_enabled() {
        let mut settings = mm_model::config::RateLimitSettings::default();
        for enable in [None, Some(false)] {
            settings.enable = enable;
            let limits = RateLimits::from_config(&settings, &[]);
            assert!(limits.global.is_none());
            for route in [Route::Login, Route::DesktopToken, Route::OAuthRegister] {
                assert!(limits.route(route).is_none(), "{route:?}");
            }
        }
        settings.enable = Some(true);
        settings.per_sec = Some(3);
        settings.max_burst = Some(4);
        let limits = RateLimits::from_config(&settings, &["X-Forwarded-For".to_owned()]);
        let global = limits.global.as_ref().expect("built");
        assert_eq!(global.gcra.limit, 5);
        assert_eq!(global.trusted_proxy_ip_header, ["X-Forwarded-For"]);
        let limit = |route| limits.route(route).expect("built").gcra.limit;
        assert_eq!(limit(Route::Login), 11);
        assert_eq!(limit(Route::DesktopToken), 2);
        assert_eq!(limit(Route::OAuthRegister), 2);
        // The route limiters take no trusted header, whatever the configuration says.
        let login = limits.route(Route::Login).expect("built");
        assert!(login.trusted_proxy_ip_header.is_empty());
        let peer: SocketAddr = "10.0.0.1:1".parse().expect("an address");
        let xff = parts("/", &[("X-Forwarded-For", "1.2.3.4")]);
        assert_eq!(login.generate_key(&xff, Some(peer)), "10.0.0.1");
        assert_eq!(global.generate_key(&xff, Some(peer)), "1.2.3.4");
    }

    #[test]
    fn an_invalid_quota_is_refused() {
        assert!(Gcra::new(0, 1, 1).is_err());
        assert!(Gcra::new(-1, 1, 1).is_err());
        assert!(Gcra::new(1, -1, 1).is_err());
        assert!(Gcra::new(1, 0, 0).is_ok(), "an unbounded store is legal");
    }

    /// The global set goes first; on a forwarded answer Go's own first value is dropped.
    #[test]
    fn the_global_set_is_prepended_and_replaces_gos() {
        let ours = [(X_RATELIMIT_LIMIT, HeaderValue::from(101))];
        let mut served = HeaderMap::new();
        served.append(X_RATELIMIT_LIMIT, HeaderValue::from(11));
        prepend(&mut served, &ours, false);
        let values: Vec<_> = served.get_all(X_RATELIMIT_LIMIT).iter().collect();
        assert_eq!(values, ["101", "11"]);

        let mut forwarded = HeaderMap::new();
        forwarded.append(X_RATELIMIT_LIMIT, HeaderValue::from(7));
        forwarded.append(X_RATELIMIT_LIMIT, HeaderValue::from(11));
        prepend(&mut forwarded, &ours, true);
        let values: Vec<_> = forwarded.get_all(X_RATELIMIT_LIMIT).iter().collect();
        assert_eq!(values, ["101", "11"]);

        let mut bare = HeaderMap::new();
        prepend(&mut bare, &ours, true);
        let values: Vec<_> = bare.get_all(X_RATELIMIT_LIMIT).iter().collect();
        assert_eq!(values, ["101"]);
    }

    #[test]
    fn seconds_round_up() {
        assert_eq!(ceil_seconds(0), 0);
        assert_eq!(ceil_seconds(1), 1);
        assert_eq!(ceil_seconds(SECOND_NS), 1);
        assert_eq!(ceil_seconds(SECOND_NS + 1), 2);
        assert_eq!(ceil_seconds(2_200_000_000), 3);
    }

    /// `fixtures/behaviour_ratelimit.json`, produced by throttled's own GCRA and store and by
    /// Mattermost's own `RateLimiter` (`reference/dump/behaviour_ratelimit.go`).
    mod go_parity {
        use super::*;

        fn oracle() -> serde_json::Value {
            serde_json::from_str(include_str!("../../../fixtures/behaviour_ratelimit.json"))
                .expect("behaviour_ratelimit.json is generated by reference/dump")
        }

        #[test]
        fn the_gcra_matches_throttled_step_for_step() {
            let oracle = oracle();
            let cases = oracle["gcra"].as_array().expect("cases");
            assert!(cases.len() >= 10);
            let mut limited_steps = 0;
            for case in cases {
                let name = case["name"].as_str().unwrap();
                let gcra = Gcra::new(
                    case["per_sec"].as_i64().unwrap(),
                    case["max_burst"].as_i64().unwrap(),
                    case["store_size"].as_i64().unwrap(),
                )
                .unwrap();
                for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                    let (limited, got) = gcra.rate_limit(
                        step["key"].as_str().unwrap(),
                        step["at_ns"].as_i64().unwrap(),
                    );
                    let want = RateLimitResult {
                        limit: step["limit"].as_i64().unwrap(),
                        remaining: step["remaining"].as_i64().unwrap(),
                        reset_after: step["reset_ns"].as_i64().unwrap(),
                        retry_after: step["retry_after_ns"].as_i64().unwrap(),
                    };
                    assert_eq!(limited, step["limited"].as_bool().unwrap(), "{name} #{i}");
                    assert_eq!(got, want, "{name} #{i}");
                    limited_steps += usize::from(limited);
                }
            }
            assert!(limited_steps >= 10, "both verdicts are exercised");
        }

        #[tokio::test]
        async fn the_writer_headers_and_refusal_match_go() {
            let oracle = oracle();
            for case in oracle["writer"].as_array().expect("cases") {
                let name = case["name"].as_str().unwrap();
                let limiter = RateLimiter::new(
                    &Settings::route(
                        case["per_sec"].as_i64().unwrap(),
                        case["max_burst"].as_i64().unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap();
                for (i, want) in case["responses"].as_array().unwrap().iter().enumerate() {
                    // A burst replayed at one instant; see the oracle's notes.
                    let verdict = limiter.verdict_at("10.0.0.1", 0);
                    assert_eq!(
                        verdict.limited,
                        want["limited"].as_bool().unwrap(),
                        "{name} #{i}"
                    );
                    let (headers, body, status) = if verdict.limited {
                        let response = bare_limit_exceeded(&verdict.headers);
                        let status = response.status().as_u16();
                        let mut headers = response.headers().clone();
                        headers.remove(crate::error::SERVED_BY);
                        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        (headers, String::from_utf8(body.to_vec()).unwrap(), status)
                    } else {
                        let mut headers = HeaderMap::new();
                        add_headers(&mut headers, &verdict.headers);
                        (headers, String::new(), 0)
                    };
                    let mut got: Vec<(String, String)> = headers
                        .iter()
                        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
                        .collect();
                    got.sort();
                    let mut expected: Vec<(String, String)> = want["headers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|pair| {
                            (
                                pair[0].as_str().unwrap().to_ascii_lowercase(),
                                pair[1].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect();
                    expected.sort();
                    assert_eq!(got, expected, "{name} #{i}: headers");
                    assert_eq!(body, want["body"].as_str().unwrap(), "{name} #{i}: body");
                    assert_eq!(
                        u64::from(status),
                        want["status"].as_u64().unwrap(),
                        "{name} #{i}"
                    );
                }
            }
        }

        #[test]
        fn every_key_matches_generate_key() {
            let oracle = oracle();
            let rows = oracle["generate_key"].as_array().expect("rows");
            assert!(rows.len() >= 100);
            for row in rows {
                let s = &row["settings"];
                let settings = Settings {
                    vary_by_user: s["vary_by_user"].as_bool().unwrap(),
                    vary_by_remote_addr: s["vary_by_remote_addr"].as_bool().unwrap(),
                    vary_by_header: s["vary_by_header"].as_str().unwrap().to_owned(),
                    ..Settings::route(10, 100)
                };
                let trusted = s["trusted"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_owned())
                    .collect();
                let limiter = RateLimiter::new(&settings, trusted).unwrap();
                let headers: Vec<(&str, &str)> = row["headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pair| (pair[0].as_str().unwrap(), pair[1].as_str().unwrap()))
                    .collect();
                let request = parts(row["target"].as_str().unwrap(), &headers);
                let peer = row["remote_addr"].as_str().unwrap().parse().ok();
                assert_eq!(
                    limiter.generate_key(&request, peer),
                    row["want"].as_str().unwrap(),
                    "{} under {s}",
                    row["name"]
                );
            }
        }
    }
}
