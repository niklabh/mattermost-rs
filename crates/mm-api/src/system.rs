//! The migrated `/api/v4/system/*` reads plus `GET /api/v4/cluster/status`.
//!
//! Five routes with one thing in common: each is a read whose answer is decided by a table, the
//! configuration, or a boundary — none of them touches a user, a team or a channel, which is why
//! they are one module rather than five.
//!
//! # `/system/ping` is the only unauthenticated route this server answers
//!
//! It is registered as `api.APIHandler`, not `APISessionRequired` (system.go:40), so it takes no
//! session and returns no 401. Every other handler here is admin-gated.

use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_READ_ENVIRONMENT_HIGH_AVAILABILITY,
    SYSCONSOLE_READ_PERMISSIONS, make_permission_error,
};
use mm_model::system::ServerBusyState;
use mm_model::utils::AppError;
use serde_json::Value;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `model.STATUS`, `model.StatusOk` and `model.StatusUnhealthy`.
///
/// # Why these three are declared here rather than in `mm-model`
///
/// They are in `model/client4.go` (lines 45-49), the 8,526-line Go REST client that CLAUDE.md
/// puts out of scope and off-limits to read. Only these three constants leak out of it into a
/// server-side handler, and only into this one, so a `mm-model` module would exist to hold three
/// strings nothing else imports. Their values were taken by name-targeted grep, not by reading
/// the file, and they are asserted in [`tests`].
///
/// `STATUS_KEY` is both the JSON key and — on the `get_server_status` branch, which this server
/// forwards — the response *header* name.
const STATUS_KEY: &str = "status";
/// See [`STATUS_KEY`].
const STATUS_OK: &str = "OK";
/// See [`STATUS_KEY`]. Emitted only on branches this server forwards; kept so that the tests can
/// state what we are choosing not to answer, which is why it is `cfg(test)` rather than dead.
#[cfg(test)]
const STATUS_UNHEALTHY: &str = "UNHEALTHY";

/// The query parameters `getSystemPing` reads, all three of which are boundaries here.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PingParams {
    /// `r.FormValue("get_server_status")`, compared against the literal `"true"`.
    get_server_status: Option<String>,
    /// `r.FormValue("device_id")`, non-empty.
    device_id: Option<String>,
    /// `r.FormValue("use_rest_semantics")` — only consulted on an unhealthy answer.
    #[allow(
        dead_code,
        reason = "read by Go only on a branch we forward; see the handler"
    )]
    use_rest_semantics: Option<String>,
}

/// Port of `getSystemPing` (api4/system.go:333).
///
/// # What we answer, and what we hand back
///
/// The plain ping is four config strings, a status and the active search backend — all readable
/// from the configuration document both servers share. Three inputs take it out of reach and each
/// is forwarded rather than guessed:
///
/// - **`get_server_status=true`** runs `DBHealthCheckWrite`/`Delete` and
///   `TestFileStoreConnection`, and then reports whether the *Go process* is running as root
///   (`os.Geteuid()`). Our euid is not the answer to that question.
/// - **`device_id=…`** sends a real push notification through the push proxy and reports whether
///   it landed. There is no push client here.
/// - **a configured `GoroutineHealthThreshold`** compares `runtime.NumGoroutine()` against it.
///   That is the Go process's goroutine count; ours is a different number about a different
///   program, and reporting `UNHEALTHY` — or failing to — on the strength of it would be worse
///   than not answering. The default is **-1**, so this forwards only where an operator has
///   deliberately armed the check.
///
/// A licensed installation is forwarded too, because an enterprise build may have registered an
/// Elasticsearch engine and `ActiveSearchBackend` would then be its name rather than
/// `"database"`. So is a config that enables Elasticsearch searching at all, licence or not —
/// belt and braces on the one field here we cannot verify from the config alone.
///
/// # `TestFeatureFlag` is a key that appears, not a value that changes
///
/// Present only when `FeatureFlags.TestFeature != "off"` (system.go:346). Go strips the
/// `FeatureFlags` section before persisting the config, so the environment is its only source and
/// the default is `"off"` — see [`mm_app::config::Config::feature_flag_test_feature`].
///
/// # Wire format: `model.ToJSON`, so no trailing newline, and sorted keys
///
/// The body is a `map[string]any` marshalled by `encoding/json`, which sorts map keys **by
/// byte**. `"status"` is lower-case and therefore sorts *after* every capitalised key —
/// `ActiveSearchBackend`, `AndroidLatestVersion`, … , `status`. A [`BTreeMap<String, Value>`]
/// orders identically, which is why the body is built as one rather than as a struct: a struct
/// would emit declaration order and put `status` first.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn get_system_ping(
    State(state): State<AppState>,
    Query(params): Query<PingParams>,
    request: Request,
) -> Response {
    match ping_answer(&state, &params).await {
        Ok(Some(response)) => response,
        Ok(None) => proxy::forward_to_go(State(state), request).await,
        Err(err) => err.into_response(),
    }
}

/// The ping's answer, or [`None`] when this ping belongs to Go.
///
/// Split out of [`get_system_ping`] because the **local-mode** router registers the same Go
/// handler (`system_local.go:15`) and must forward over the unix socket rather than over TCP —
/// see [`crate::local`]. Returning the decision instead of acting on it is what lets one body of
/// logic serve two transports; the alternative was a second copy of the five boundaries, which is
/// exactly the duplication that drifts.
///
/// Records `forwarded` on the **caller's** span, so it is deliberately not instrumented itself.
pub(crate) async fn ping_answer(
    state: &AppState,
    params: &PingParams,
) -> Result<Option<Response>, ApiError> {
    let config = state.app.config();

    let licensed = state.app.license_state().await? == mm_app::license::LicenseState::Licensed;

    if let Some(reason) = ping_is_not_ours_to_answer(
        params.get_server_status.as_deref(),
        params.device_id.as_deref(),
        config.goroutine_health_threshold,
        licensed,
        config.elasticsearch_enable_searching,
    ) {
        tracing::Span::current().record("forwarded", reason);
        return Ok(None);
    }
    tracing::Span::current().record("forwarded", "");

    let mut body: BTreeMap<String, Value> = BTreeMap::new();
    body.insert(STATUS_KEY.to_owned(), Value::from(STATUS_OK));
    body.insert(
        "AndroidLatestVersion".to_owned(),
        Value::from(config.android_latest_version.as_str()),
    );
    body.insert(
        "AndroidMinVersion".to_owned(),
        Value::from(config.android_min_version.as_str()),
    );
    body.insert(
        "IosLatestVersion".to_owned(),
        Value::from(config.ios_latest_version.as_str()),
    );
    body.insert(
        "IosMinVersion".to_owned(),
        Value::from(config.ios_min_version.as_str()),
    );
    if config.feature_flag_test_feature != "off" {
        body.insert(
            "TestFeatureFlag".to_owned(),
            Value::from(config.feature_flag_test_feature.as_str()),
        );
    }
    body.insert(
        "ActiveSearchBackend".to_owned(),
        Value::from(active_search_backend(config.disable_database_search)),
    );

    match serde_json::to_vec(&body) {
        // `model.ToJSON` is a bare `json.Marshal` — **no** trailing newline, unlike the
        // `json.NewEncoder(w).Encode` every neighbouring route uses.
        Ok(body) => Ok(Some(json_body(body))),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the ping");
            Err(ApiError::from(AppError::new(
                "getSystemPing",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )))
        }
    }
}

/// A 200 carrying `body` verbatim, with the two headers every locally-served response needs.
///
/// `Content-Type: application/json` is set by `web.Handler.ServeHTTP` for every non-static API
/// response (handlers.go:259), **before** the handler runs — so it is not the handler's choice and
/// not conditional on the body. `x-mmrs-served-by` is this project's cutover marker; the parity
/// suite fails a comparison that silently measured Go twice without it.
fn json_body(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

/// Why this ping belongs to Go, or [`None`] when it does not.
///
/// A named function over plain values rather than five conditions inline, because four of the
/// five are **unreachable from the parity suite** — nothing it can do arms a goroutine threshold
/// or installs a push proxy — so a mutation deleting one survives every stack test. Here they
/// have a truth table.
fn ping_is_not_ours_to_answer(
    get_server_status: Option<&str>,
    device_id: Option<&str>,
    goroutine_health_threshold: i64,
    licensed: bool,
    elasticsearch_enable_searching: bool,
) -> Option<&'static str> {
    // Go compares against the literal `"true"`; `?get_server_status=1` is not a request for the
    // extended check and must stay ours.
    if get_server_status == Some("true") {
        return Some("get_server_status");
    }
    // `if deviceID := r.FormValue("device_id"); deviceID != ""` — an **empty** value is not a
    // device id, so `?device_id=` alone stays ours.
    if device_id.is_some_and(|id| !id.is_empty()) {
        return Some("device_id");
    }
    // `> 0`, not `!= 0`: the default is -1 and Go's own guard is a strict positive.
    if goroutine_health_threshold > 0 {
        return Some("goroutine_health_threshold");
    }
    if licensed {
        return Some("licensed");
    }
    if elasticsearch_enable_searching {
        return Some("elasticsearch");
    }
    None
}

/// Port of `Broker.ActiveEngine` (platform/services/searchengine/searchengine.go:42) for the
/// case where no engine is registered.
///
/// The broker holds exactly one optional engine — Elasticsearch — and registers it only from
/// `initEnterprise` (platform/service.go:548), which the Team Edition binary never reaches. With
/// no active engine the answer is `"none"` when database search is disabled and `"database"`
/// otherwise. The caller forwards before reaching here if either a licence or the Elasticsearch
/// setting could make that untrue.
fn active_search_backend(disable_database_search: bool) -> &'static str {
    if disable_database_search {
        "none"
    } else {
        "database"
    }
}

/// Port of `getSupportedTimezones` (api4/system.go:1064).
///
/// # It is a compile-time table, not the host's tzdata
///
/// `App.Timezones().GetSupported()` returns `DefaultSupportedTimezones`, a literal array in the
/// Go source — so both servers answer from the same 592 strings and neither consults the
/// operating system. Contrast [D-065], where `time.LoadLocation` does read the host and therefore
/// has no single right answer.
///
/// # `nil` becomes `[]`, and here it cannot be either
///
/// Go guards with `if supportedTimezones == nil { … make([]string, 0) }` before marshalling. The
/// table is never nil, so the guard is dead — but it is what fixes the empty answer as `[]`
/// rather than `null`, and this port inherits the shape by holding a slice that is never null.
///
/// `json.Marshal` and a bare `w.Write`: **no** trailing newline.
#[tracing::instrument(skip_all, fields(count))]
pub async fn get_supported_timezones(_session: AuthenticatedSession) -> Result<Response, ApiError> {
    let zones = mm_model::timezones::Timezones::new();
    let supported = zones.get_supported();
    tracing::Span::current().record("count", supported.len());

    let body = serde_json::to_vec(supported).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the timezone list");
        ApiError::from(AppError::new(
            "getSupportedTimezones",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Port of `getAppliedSchemaMigrations` (api4/system.go:1076).
///
/// # The permission is "any sysconsole **read**", which is 30-odd permissions
///
/// `SessionHasPermissionToAny(SysconsoleReadPermissions)` — so any role holding a single
/// system-console read permission may list the schema migrations, not only a system admin. The
/// refusal names the whole list.
///
/// `json.Marshal` and `w.Write`: **no** trailing newline, unlike `getOnboarding` beside it.
#[tracing::instrument(skip_all, fields(count))]
pub async fn get_applied_schema_migrations(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to_any(&session.0, SYSCONSOLE_READ_PERMISSIONS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            SYSCONSOLE_READ_PERMISSIONS,
        )));
    }

    let migrations = state.app.get_applied_schema_migrations().await?;
    tracing::Span::current().record("count", migrations.len());

    let body = serde_json::to_vec(&migrations).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the migration list");
        ApiError::from(AppError::new(
            "getAppliedMigrations",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Port of `getOnboarding` (api4/system.go:1010).
///
/// `manage_system`, and a `model.System` row — synthesised as `"false"` when the row is absent.
/// See [`mm_app::App::get_onboarding`], which holds that decision.
///
/// `json.NewEncoder(w).Encode`: **trailing newline**, unlike the two routes above it in this
/// module. The three sit within sixty lines of each other in Go and use two different writers.
#[tracing::instrument(skip_all)]
pub async fn get_onboarding(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let onboarding = state.app.get_onboarding().await?;
    let mut body = serde_json::to_vec(&onboarding).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the onboarding row");
        ApiError::from(AppError::new(
            "getOnboarding",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Port of `getClusterStatus` (api4/cluster.go:18).
///
/// # The answer is `[]`, and that is the whole route on an unlicensed server
///
/// `App.GetClusterStatus` returns `make([]*model.ClusterInfo, 0)` whenever no cluster interface
/// is registered, which on Team Edition is always — see [`mm_app::App::get_cluster_status`]. A
/// licensed installation is forwarded: the roster is gossip state held in the other process and
/// there is no way to read it from here.
///
/// # `SessionHasPermissionToAndNotRestrictedAdmin`
///
/// Not the plain check. With `ExperimentalSettings.RestrictSystemAdmin` set, a system admin is
/// refused even though the permission is granted — the setting exists to lock a hosted
/// installation's admin out of infrastructure routes, and this is one of them.
///
/// `json.Marshal` and `w.Write`: **no** trailing newline.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_cluster_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match state.app.cluster_status_is_ours_to_answer().await {
        Ok(true) => tracing::Span::current().record("licensed", false),
        Ok(false) => {
            tracing::Span::current().record("licensed", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_ENVIRONMENT_HIGH_AVAILABILITY,
        )
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_ENVIRONMENT_HIGH_AVAILABILITY],
        ))
        .into_response();
    }

    let infos = match state.app.get_cluster_status().await {
        Ok(infos) => infos,
        Err(err) => return ApiError::from(err).into_response(),
    };

    match serde_json::to_vec(&infos) {
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
            tracing::error!(error = %err, "failed to serialise the cluster roster");
            ApiError::from(AppError::new(
                "getClusterStatus",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `DefaultServerBusySeconds` (api4/system.go:33).
const DEFAULT_SERVER_BUSY_SECONDS: i64 = 3600;
/// `MaxServerBusySeconds` (api4/system.go:34).
const MAX_SERVER_BUSY_SECONDS: i64 = 86400;

/// `time.Time{}.Unix()` — the zero `time.Time` is 0001-01-01T00:00:00Z, which is this many
/// seconds *before* the epoch.
///
/// It reaches the wire because `Busy.ToJSON` (platform/busy.go:137) reads `b.expires`
/// unconditionally, and a server that has never been marked busy holds the zero time there. So
/// the ordinary, overwhelmingly common answer to `GET /api/v4/server_busy` carries a negative
/// nine-hundred-billion. Measured against the running Go server over its own socket, not derived.
const GO_ZERO_TIME_UNIX: i64 = -62135596800;

/// The same instant through `TimestampFormat` (platform/busy.go:18).
///
/// Go's reference layout is `Mon Jan 2 15:04:05 -0700 MST 2006`. Two details a port gets wrong:
/// the **day is not zero-padded** (`2` is `stdDay`) while the **year is** (`2006` is
/// `stdLongYear`, `appendInt(…, 4)`), so year 1 renders as `0001` beside a bare `1` for the day.
/// Measured, like [`GO_ZERO_TIME_UNIX`].
const GO_ZERO_TIME_TS: &str = "Mon Jan 1 00:00:00 +0000 UTC 0001";

/// Port of `platform.Busy` (channels/app/platform/busy.go:23) — the server's busy flag.
///
/// # Why this is a process global and not a field on [`AppState`]
///
/// Because that is what it is in Go: one `Busy` on the `Server`, reached as `Srv().Platform()
/// .Busy`. It is deliberately *not* persisted — no row, no cache entry — which is the whole of
/// [D-320]: a busy state set through this server is invisible to the Go process beside it, and
/// vice versa. Putting it on `AppState` would imply a per-request or per-connection scope it does
/// not have.
///
/// # One field where Go has three
///
/// Go keeps an atomic flag, an expiry and a `time.AfterFunc` timer that clears both. Here the
/// expiry is the whole state and "busy" is derived from it, because the timer exists only to make
/// the flag agree with the clock — and a reader that compares against the clock already agrees.
/// The observable difference is nil: Go's `ToJSON` runs under the same mutex the timer takes, so
/// no caller can see a fired deadline with the flag still set.
#[derive(Debug)]
pub struct ServerBusy {
    /// When the busy state lapses, or [`None`] when the server has never been marked busy or has
    /// been cleared. Never a past instant *observably* — [`ServerBusy::state`] reads an elapsed
    /// deadline as cleared.
    expires: Mutex<Option<DateTime<Utc>>>,
}

impl ServerBusy {
    const fn new() -> Self {
        Self {
            expires: Mutex::new(None),
        }
    }

    /// Port of `Busy.Set` (busy.go:46).
    ///
    /// Go floors the duration at one second. The handler already rejects anything below 1, so the
    /// floor is unreachable from the REST API — it is ported because `Set` is also called from
    /// `ClusterEventChanged`, and because a mutation that dropped it would otherwise survive.
    ///
    /// **Not ported: the cluster notification.** Go sends a `CLUSTER_EVENT_BUSY_STATE_CHANGED`
    /// message when a cluster interface is registered; Team Edition registers none, so the branch
    /// is dead there too.
    fn set(&self, seconds: i64, now: DateTime<Utc>) {
        let seconds = seconds.max(1);
        let Some(delta) = TimeDelta::try_seconds(seconds) else {
            // `try_seconds` is `None` only past ~292 billion years. The handler caps at 86400.
            tracing::error!(seconds, "busy duration out of range; ignoring");
            return;
        };
        if let Ok(mut expires) = self.expires.lock() {
            *expires = Some(now + delta);
        }
    }

    /// Port of `Busy.Clear` (busy.go:76).
    fn clear(&self) {
        if let Ok(mut expires) = self.expires.lock() {
            *expires = None;
        }
    }

    /// Port of `Busy.ToJSON`'s state half (busy.go:137).
    ///
    /// The comparison is `now < expires`, not `<=`: Go's timer fires *at* the deadline and clears
    /// the flag, so the deadline instant itself is already not busy.
    fn state(&self, now: DateTime<Utc>) -> ServerBusyState {
        let expires = self
            .expires
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .filter(|expires| now < *expires);

        match expires {
            Some(expires) => ServerBusyState {
                busy: true,
                expires: expires.timestamp(),
                expires_ts: go_timestamp(expires),
            },
            None => ServerBusyState {
                busy: false,
                expires: GO_ZERO_TIME_UNIX,
                expires_ts: GO_ZERO_TIME_TS.to_owned(),
            },
        }
    }
}

/// This process's busy flag. See [`ServerBusy`] for why it is a global.
static SERVER_BUSY: ServerBusy = ServerBusy::new();

/// `t.UTC().Format(platform.TimestampFormat)`.
///
/// The zone is always UTC here — `ToJSON` calls `.UTC()` first — so the offset and the
/// abbreviation are constants rather than `%z`/`%Z`: Go would render them `+0000` and `UTC`, and
/// chrono's `%Z` on a `DateTime<Utc>` is not guaranteed to be that string. `%-d` is the
/// unpadded day Go's `2` means.
fn go_timestamp(at: DateTime<Utc>) -> String {
    at.format("%a %b %-d %H:%M:%S +0000 UTC %Y").to_string()
}

/// Port of `web.ReturnStatusOK` (web/web.go:127) — `{"status":"OK"}`, **no trailing newline**.
///
/// A second copy of `auth_writes::status_ok`; the two are in different modules because neither is
/// the natural home for the other's routes, and the body is fifteen bytes fixed by Go.
fn status_ok() -> Response {
    json_body(br#"{"status":"OK"}"#.to_vec())
}

/// Port of `setServerBusy` (api4/system.go:807).
///
/// # The permission check comes first, and the order is load-bearing
///
/// Go checks `manage_system` **before** it looks at `?seconds=`, so a caller without rights gets
/// 403 for a request that is also malformed. Swapping the two leaks to an unauthorised caller
/// whether their parameter would have been accepted.
///
/// # `seconds` is a URL param whose *name*, on the error, is a whole sentence
///
/// `c.SetInvalidURLParam(fmt.Sprintf("seconds must be 1 - %d", MaxServerBusySeconds))` — the
/// format string is passed where every other call site passes a parameter name, so the rendered
/// message reads "Invalid or missing seconds must be 1 - 86400 parameter in request URL." That is
/// not a typo to fix: it is the string clients see, and `Name` in the params map carries it.
///
/// Absent **and** empty both mean the 3600-second default: Go reads `Query().Get`, which cannot
/// tell `?seconds=` from no parameter at all.
#[tracing::instrument(skip_all, fields(seconds))]
pub async fn set_server_busy(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let raw = crate::channels::query_first(request.uri().query(), "seconds").unwrap_or_default();
    let seconds = parse_busy_seconds(&raw).ok_or_else(invalid_seconds)?;
    tracing::Span::current().record("seconds", seconds);

    SERVER_BUSY.set(seconds, Utc::now());
    tracing::warn!(
        seconds,
        "server busy state activated - non-critical services disabled"
    );

    Ok(status_ok())
}

/// The whole of `setServerBusy`'s parameter handling, as a function of the raw query value.
///
/// Named, and taking a `&str` rather than living inline, because three of its four rejections are
/// awkward to reach through a socket-backed parity test and one — the empty-string default — is
/// indistinguishable from success in the response body. Here they have a truth table.
fn parse_busy_seconds(raw: &str) -> Option<i64> {
    let raw = if raw.is_empty() {
        // `strconv.FormatInt(DefaultServerBusySeconds, 10)`: Go substitutes the default as a
        // *string* and then parses it, so the default goes through the bounds check like any
        // other value.
        DEFAULT_SERVER_BUSY_SECONDS.to_string()
    } else {
        raw.to_owned()
    };
    // `strconv.ParseInt(secs, 10, 64)`. A leading `+` is accepted by both, `0x10` by neither.
    let parsed: i64 = raw.parse().ok()?;
    // `i <= 0 || i > MaxServerBusySeconds` — inclusive at 86400, exclusive at 0.
    if parsed <= 0 || parsed > MAX_SERVER_BUSY_SECONDS {
        return None;
    }
    Some(parsed)
}

/// `NewInvalidURLParamError("seconds must be 1 - 86400")`. See [`set_server_busy`].
fn invalid_seconds() -> ApiError {
    ApiError::invalid_url_param(&format!("seconds must be 1 - {MAX_SERVER_BUSY_SECONDS}"))
}

/// Port of `clearServerBusy` (api4/system.go:836).
#[tracing::instrument(skip_all)]
pub async fn clear_server_busy(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    SERVER_BUSY.clear();
    tracing::info!("server busy state cleared - non-critical services enabled");

    Ok(status_ok())
}

/// Port of `getServerBusyExpires` (api4/system.go:852).
///
/// `w.Write(sbsJSON)` on the bytes `Busy.ToJSON` marshalled — so **no trailing newline**, and the
/// field order is `model.ServerBusyState`'s declaration order rather than sorted, because it is a
/// struct and not a map.
#[tracing::instrument(skip_all, fields(busy))]
pub async fn get_server_busy_expires(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let busy = SERVER_BUSY.state(Utc::now());
    tracing::Span::current().record("busy", busy.busy);

    let body = serde_json::to_vec(&busy).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the busy state");
        ApiError::from(AppError::new(
            "getServerBusyExpires",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok(json_body(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?seconds=` boundaries, every branch, in Go's order.
    ///
    /// Three of the four rejections produce the same 400 with the same body, so a socket-backed
    /// test cannot tell them apart; a truth table can. `0` and `86401` are the pair that pins the
    /// comparison operators — `<= 0` rejects zero while `> 86400` accepts 86400.
    #[test]
    fn the_seconds_parameter_accepts_exactly_one_to_the_maximum() {
        assert_eq!(
            parse_busy_seconds(""),
            Some(DEFAULT_SERVER_BUSY_SECONDS),
            "absent and empty both take the 3600-second default"
        );
        assert_eq!(
            parse_busy_seconds("1"),
            Some(1),
            "the lower bound is inclusive"
        );
        assert_eq!(
            parse_busy_seconds("86400"),
            Some(MAX_SERVER_BUSY_SECONDS),
            "the upper bound is inclusive: `i > MaxServerBusySeconds`, not `>=`"
        );

        assert_eq!(parse_busy_seconds("0"), None, "`i <= 0` rejects zero");
        assert_eq!(parse_busy_seconds("-1"), None);
        assert_eq!(parse_busy_seconds("86401"), None);
        assert_eq!(parse_busy_seconds("abc"), None, "ParseInt fails");
        assert_eq!(
            parse_busy_seconds("1.5"),
            None,
            "ParseInt is not a float parser"
        );
        assert_eq!(parse_busy_seconds("0x10"), None, "base 10, explicitly");
        assert_eq!(parse_busy_seconds(" 1"), None, "ParseInt does not trim");
    }

    /// The rejection's `Name` is a whole sentence, and the id is the **URL** one.
    ///
    /// Both halves are wire format: the webapp branches on `id`, and the rendered message
    /// interpolates `Name`. Measured against the running Go server over its own socket:
    /// "Invalid or missing seconds must be 1 - 86400 parameter in request URL."
    #[test]
    fn the_seconds_rejection_names_the_whole_range() {
        let err = invalid_seconds();
        assert_eq!(err.0.id, "api.context.invalid_url_param.app_error");
        assert_eq!(err.0.status_code, 400);
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::Value::String(
                "seconds must be 1 - 86400".to_owned()
            )),
            "the format string is passed where a parameter name belongs; that is Go's, not a typo"
        );
    }

    /// A server that has never been marked busy answers with the zero `time.Time`, twice over.
    ///
    /// This is the overwhelmingly common answer and the one a port invents instead of measuring:
    /// a `0` epoch and an empty `expires_ts` would both round-trip through
    /// `model.ServerBusyState` and both be wrong. The values are the running Go server's, read
    /// over its own socket.
    #[test]
    fn an_idle_server_reports_the_go_zero_time() {
        let busy = ServerBusy::new();
        let state = busy.state(Utc::now());

        assert!(!state.busy);
        assert_eq!(
            state.expires, -62135596800,
            "the zero time.Time, in seconds before the epoch"
        );
        assert_eq!(state.expires_ts, "Mon Jan 1 00:00:00 +0000 UTC 0001");

        assert_eq!(
            serde_json::to_string(&state).expect("serialises"),
            r#"{"busy":false,"expires":-62135596800,"expires_ts":"Mon Jan 1 00:00:00 +0000 UTC 0001"}"#,
            "field order is the struct's, not sorted — ServerBusyState is a struct, not a map"
        );
    }

    /// Set, read, clear, read — and the expiry arithmetic in between.
    #[test]
    fn setting_and_clearing_move_the_expiry() {
        let now = DateTime::parse_from_rfc3339("2026-09-11T15:03:12Z")
            .expect("valid")
            .with_timezone(&Utc);
        let busy = ServerBusy::new();

        busy.set(1, now);
        let state = busy.state(now);
        assert!(state.busy);
        assert_eq!(state.expires, now.timestamp() + 1);
        assert_eq!(
            state.expires_ts, "Fri Sep 11 15:03:13 +0000 UTC 2026",
            "the day is unpadded and the zone is a literal +0000 UTC"
        );

        busy.clear();
        assert!(!busy.state(now).busy, "Clear zeroes the expiry");
    }

    /// A **single-digit** day, which is the only input that separates Go's `2` from a padded one.
    ///
    /// Every plausible test date is two-digit for two-thirds of the month, so `%d` and `%-d` agree
    /// and a mutation between them survives. This is the third of the month on purpose.
    #[test]
    fn the_day_of_the_month_is_not_zero_padded() {
        let now = DateTime::parse_from_rfc3339("2026-03-03T04:05:06Z")
            .expect("valid")
            .with_timezone(&Utc);
        let busy = ServerBusy::new();
        busy.set(1, now);
        assert_eq!(
            busy.state(now).expires_ts,
            "Tue Mar 3 04:05:07 +0000 UTC 2026",
            "`Jan 2` is stdDay — unpadded — while `15:04:05` is padded"
        );
    }

    /// The deadline instant is **not** busy: Go's timer fires at it and clears the flag.
    ///
    /// One second either side of the boundary, because `<` and `<=` are otherwise
    /// indistinguishable from any test that does not sit exactly on it.
    #[test]
    fn the_expiry_instant_has_already_lapsed() {
        let now = DateTime::parse_from_rfc3339("2026-09-11T15:03:12Z")
            .expect("valid")
            .with_timezone(&Utc);
        let busy = ServerBusy::new();
        busy.set(10, now);

        assert!(
            busy.state(now + TimeDelta::seconds(9)).busy,
            "before the deadline"
        );
        assert!(
            !busy.state(now + TimeDelta::seconds(10)).busy,
            "at the deadline the timer has fired"
        );
        assert!(
            !busy.state(now + TimeDelta::seconds(11)).busy,
            "and after it"
        );
    }

    /// `Busy.Set` floors the duration at one second (busy.go:52).
    ///
    /// Unreachable through the REST API — the handler rejects anything below 1 — so this is the
    /// only place the floor is held down at all.
    #[test]
    fn the_duration_is_floored_at_one_second() {
        let now = DateTime::parse_from_rfc3339("2026-09-11T15:03:12Z")
            .expect("valid")
            .with_timezone(&Utc);
        let busy = ServerBusy::new();
        busy.set(0, now);
        assert_eq!(
            busy.state(now).expires,
            now.timestamp() + 1,
            "a zero duration still marks the server busy for a second"
        );
    }

    /// `{"status":"OK"}` with no trailing newline, and the cutover marker.
    #[test]
    fn the_status_ok_body_is_fifteen_bytes() {
        let body = br#"{"status":"OK"}"#;
        assert_eq!(body.len(), 15);
        assert_ne!(body[body.len() - 1], b'\n', "w.Write, not an encoder");
    }

    /// The default deployment answers, and none of the five boundaries fires.
    #[test]
    fn a_stock_configuration_keeps_the_ping() {
        assert_eq!(
            ping_is_not_ours_to_answer(None, None, -1, false, false),
            None
        );
    }

    /// Each boundary on its own, and — the part that matters — the near misses that must **not**
    /// fire. Go compares `get_server_status` against the literal `"true"` and `device_id` against
    /// emptiness, so `?get_server_status=1` and `?device_id=` are ordinary pings.
    #[test]
    fn every_boundary_fires_only_on_its_own_input() {
        assert_eq!(
            ping_is_not_ours_to_answer(Some("true"), None, -1, false, false),
            Some("get_server_status")
        );
        assert_eq!(
            ping_is_not_ours_to_answer(Some("1"), None, -1, false, false),
            None,
            "`\"1\"` is not `\"true\"`; Go compares the literal"
        );
        assert_eq!(
            ping_is_not_ours_to_answer(Some("TRUE"), None, -1, false, false),
            None,
            "and the comparison is case-sensitive"
        );

        assert_eq!(
            ping_is_not_ours_to_answer(None, Some("abc"), -1, false, false),
            Some("device_id")
        );
        assert_eq!(
            ping_is_not_ours_to_answer(None, Some(""), -1, false, false),
            None,
            "an empty device id is not a device id"
        );

        assert_eq!(
            ping_is_not_ours_to_answer(None, None, 1, false, false),
            Some("goroutine_health_threshold")
        );
        assert_eq!(
            ping_is_not_ours_to_answer(None, None, 0, false, false),
            None,
            "`> 0` is strict, so a zero threshold is disarmed"
        );
        assert_eq!(
            ping_is_not_ours_to_answer(None, None, -1, false, false),
            None,
            "and -1 is the Go default"
        );

        assert_eq!(
            ping_is_not_ours_to_answer(None, None, -1, true, false),
            Some("licensed")
        );
        assert_eq!(
            ping_is_not_ours_to_answer(None, None, -1, false, true),
            Some("elasticsearch")
        );
    }

    /// The two answers `ActiveEngine` can give with no engine registered.
    #[test]
    fn the_search_backend_is_database_unless_it_is_disabled() {
        assert_eq!(active_search_backend(false), "database");
        assert_eq!(active_search_backend(true), "none");
    }

    /// The key order on the wire. `encoding/json` sorts a `map[string]any` **by byte**, so every
    /// capitalised key precedes the lower-case `status` — the opposite of what a struct with
    /// `status` declared first would emit, which is how the Go source reads.
    #[test]
    fn status_sorts_last_because_it_is_lower_case() {
        let mut body: BTreeMap<String, Value> = BTreeMap::new();
        body.insert(STATUS_KEY.to_owned(), Value::from(STATUS_OK));
        body.insert("AndroidLatestVersion".to_owned(), Value::from(""));
        body.insert("ActiveSearchBackend".to_owned(), Value::from("database"));

        let rendered = serde_json::to_string(&body).expect("encodes");
        assert_eq!(
            rendered,
            r#"{"ActiveSearchBackend":"database","AndroidLatestVersion":"","status":"OK"}"#
        );
        assert!(
            !rendered.ends_with('\n'),
            "`model.ToJSON` is a bare Marshal"
        );
    }

    /// `STATUS_UNHEALTHY` is referenced so the constant cannot rot unnoticed: every branch that
    /// would emit it is forwarded, which means nothing else in this crate mentions it.
    #[test]
    fn the_unhealthy_status_is_the_one_we_never_emit() {
        assert_eq!(STATUS_OK, "OK");
        assert_eq!(STATUS_UNHEALTHY, "UNHEALTHY");
    }
}
