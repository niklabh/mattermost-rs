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

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_SYSCONSOLE_READ_ENVIRONMENT_HIGH_AVAILABILITY,
    SYSCONSOLE_READ_PERMISSIONS, make_permission_error,
};
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
    let config = state.app.config();

    let licensed = match state.app.license_state().await {
        Ok(licence) => licence == mm_app::license::LicenseState::Licensed,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if let Some(reason) = ping_is_not_ours_to_answer(
        params.get_server_status.as_deref(),
        params.device_id.as_deref(),
        config.goroutine_health_threshold,
        licensed,
        config.elasticsearch_enable_searching,
    ) {
        tracing::Span::current().record("forwarded", reason);
        return proxy::forward_to_go(State(state), request).await;
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
            tracing::error!(error = %err, "failed to serialise the ping");
            ApiError::from(AppError::new(
                "getSystemPing",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
