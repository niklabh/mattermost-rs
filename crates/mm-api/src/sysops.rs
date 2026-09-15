//! The system-operations family of `api4/system.go` and `api4/elasticsearch.go`: the analytics
//! read, the cache and connection-pool resets, the three log routes, the restart, the three
//! enterprise-upgrade routes and the two Elasticsearch routes.
//!
//! Twelve handlers, each admin-gated, that have one thing in common: their answer is a fact
//! about **this process** (its pool, its hub, its caches, its log file, its executable, its
//! architecture) or a figure over the shared database. Two of them needed a decision about what
//! a per-process operation means while two processes serve one API, and each records it on its
//! own doc comment: [`invalidate_caches`] and [`restart`].
//!
//! Two routes of the family are not here. `POST /api/v4/notifications/test` is `CreatePost` with
//! `ForceNotification` — a flag the post family's `CreatePostFlags` does not yet carry — and
//! forwards whole ([D-680]); `GET /api/v4/system/notices/{team_id}` forwards on the notice cache
//! and its condition matcher ([D-681]).

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_app::logs::{LOG_FILENAME, LOGS_PER_PAGE_DEFAULT, LOGS_PER_PAGE_MAXIMUM};
use mm_app::searchengine::ElasticsearchTestSettings;
use mm_app::upgrader::{
    INVALID_PERMISSION, INVALID_USER, INVALID_USER_AND_PERMISSION, UpgradeError,
    can_i_upgrade_to_e0, upgrade_to_e0_status,
};
use mm_model::permission::{
    PERMISSION_GET_ANALYTICS, PERMISSION_GET_LOGS, PERMISSION_INVALIDATE_CACHES,
    PERMISSION_MANAGE_SYSTEM, PERMISSION_PURGE_ELASTICSEARCH_INDEXES,
    PERMISSION_RECYCLE_DATABASE_CONNECTIONS, PERMISSION_TEST_ELASTICSEARCH, Permission,
    make_permission_error,
};
use mm_model::system::LogFilter;
use mm_model::utils::{AppError, go_json_escape, go_json_format_float, go_json_marshal};
use mm_model::version::BUILD_ENTERPRISE_READY;
use serde_json::Value;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;
use crate::serve_content::{FileResponse, FileResponseSpec, write_file_response};

/// `web.ReturnStatusOK`: `{"status":"OK"}`, no trailing newline.
const STATUS_OK: &str = r#"{"status":"OK"}"#;

/// The `Cache-Control` `invalidateCaches` sets on its 200.
const NO_STORE: &str = "no-cache, no-store, must-revalidate";

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

fn status_ok() -> Response {
    json_response(StatusCode::OK, STATUS_OK.as_bytes().to_vec())
}

/// `c.SetPermissionError(p)` after a failed `SessionHasPermissionToAndNotRestrictedAdmin`.
async fn gate_not_restricted(
    state: &AppState,
    session: &AuthenticatedSession,
    permission: &'static Permission,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to_and_not_restricted_admin(&session.0, permission)
        .await
    {
        Ok(())
    } else {
        Err(ApiError::from(make_permission_error(
            &session.0,
            &[permission],
        )))
    }
}

/// `c.SetPermissionError(p)` after a failed `SessionHasPermissionTo`.
async fn gate(
    state: &AppState,
    session: &AuthenticatedSession,
    permission: &'static Permission,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(&session.0, permission)
        .await
    {
        Ok(())
    } else {
        Err(ApiError::from(make_permission_error(
            &session.0,
            &[permission],
        )))
    }
}

/// `r.URL.Query().Get(key)` — the first value, decoded as Go decodes it, or `""`.
fn query_get(query: Option<&str>, key: &str) -> String {
    let (values, _) = mm_model::go_url::parse_query(query.unwrap_or_default());
    values
        .get(key)
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default()
}

/// `r.URL.Query()[key]` — every value, in order.
fn query_get_all(query: Option<&str>, key: &str) -> Vec<String> {
    let (values, _) = mm_model::go_url::parse_query(query.unwrap_or_default());
    values
        .get_all(key)
        .map(|all| {
            all.iter()
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `web.Params.Page` (web/params.go:217): `strconv.Atoi`, and a failure **or a negative** is
/// page zero. The `getChannelMembersForUser` exemption is for another route.
fn page_param(query: Option<&str>) -> i64 {
    match query_get(query, "page").parse::<i64>() {
        Ok(page) if page >= 0 => page,
        _ => 0,
    }
}

/// `web.Params.LogsPerPage` (web/params.go:244): a failure or a negative is the default and
/// anything over the maximum is the maximum — both 10,000 — so only `0..=10000` passes as sent.
fn logs_per_page_param(query: Option<&str>) -> i64 {
    match query_get(query, "logs_per_page").parse::<i64>() {
        Ok(per_page) if per_page < 0 => LOGS_PER_PAGE_DEFAULT,
        Ok(per_page) if per_page > LOGS_PER_PAGE_MAXIMUM => LOGS_PER_PAGE_MAXIMUM,
        Ok(per_page) => per_page,
        Err(_) => LOGS_PER_PAGE_DEFAULT,
    }
}

// ------------------------------------------------------------------------------------------
// GET /api/v4/analytics/old
// ------------------------------------------------------------------------------------------

/// Port of `getAnalytics` (api4/system.go:498) — `GET /api/v4/analytics/old`.
///
/// `name` defaults to `standard` **before** the permission check, and the check is the plain
/// `SessionHasPermissionTo(get_analytics)` — no restricted-admin clause, unlike the rest of this
/// module. An unknown `name` is Go's `nil, nil`, which the handler reports as
/// `SetInvalidParam("name")`: a 400 blaming a *body* parameter that came from the query. The
/// rows are `json.NewEncoder(w).Encode`d, so the body ends in a newline and each `value` is a
/// Go `float64` — `12`, never `12.0`.
///
/// Three of the twelve `standard` rows are this process's own (see
/// `App::get_standard_analytics`); the parity test compares the other nine.
#[tracing::instrument(skip_all, fields(name, team_id))]
pub async fn get_analytics(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let mut name = query_get(query.as_deref(), "name");
    let team_id = query_get(query.as_deref(), "team_id");
    if name.is_empty() {
        name = "standard".to_owned();
    }
    tracing::Span::current().record("name", &name);
    tracing::Span::current().record("team_id", &team_id);

    gate(&state, &session, &PERMISSION_GET_ANALYTICS).await?;

    let rows = state.app.get_analytics(&name, &team_id).await?;
    let Some(rows) = rows else {
        return Err(ApiError::invalid_param("name"));
    };
    let mut body = go_json_marshal(&rows).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the analytics rows");
        ApiError::from(AppError::new(
            "getAnalytics",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push('\n');
    Ok(json_response(StatusCode::OK, body.into_bytes()))
}

// ------------------------------------------------------------------------------------------
// POST /api/v4/caches/invalidate, POST /api/v4/database/recycle
// ------------------------------------------------------------------------------------------

/// Port of `invalidateCaches` (api4/system.go:333) — `POST /api/v4/caches/invalidate`.
///
/// # What is invalidated, and the decision about the Go server's caches
///
/// This process drops the three things it keeps in memory (`App::invalidate_all_caches`) and
/// answers `{"status":"OK"}` under `Cache-Control: no-cache, no-store, must-revalidate`, as
/// Go does. That answer is this server's own and does not depend on Go.
///
/// It **also posts a copy of the request to the Go server**, best-effort, before answering.
/// The route exists so that an operator can make *the server* forget what it cached — and
/// while the strangler proxy is on, "the server" is two processes, and the one whose caches
/// actually go stale is Go's: every row this server writes is invisible to Go's channel, user
/// and session caches until they are purged, which is why the parity suite calls this very
/// route on Go by hand (`common::invalidate_go_caches`). A Rust answer that left Go's caches
/// standing would honour the wire format and defeat the purpose. The copy carries the caller's
/// credentials, so Go applies the same permission gate; its outcome is logged and never changes
/// this response, and when the Go server is gone the copy fails once per call and nothing else
/// changes. It goes over the proxy's own client so a request that arrived on the local socket
/// reaches Go's socket.
#[tracing::instrument(skip_all, fields(go_status))]
pub async fn invalidate_caches(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_INVALIDATE_CACHES).await?;

    state.app.invalidate_all_caches()?;

    // The Go server's copy. `forward_to_go` answers with Go's response, which is discarded
    // here after its status is recorded: the response below is this server's.
    let (mut parts, _) = request.into_parts();
    parts.headers.remove("content-length");
    let copy = Request::from_parts(parts, Body::empty());
    let go_response = proxy::forward_to_go(State(state.clone()), copy).await;
    let go_status = go_response.status();
    tracing::Span::current().record("go_status", go_status.as_u16());
    if !go_status.is_success() {
        tracing::warn!(
            status = go_status.as_u16(),
            "the Go server did not invalidate its caches alongside ours"
        );
    }

    let mut response = status_ok();
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static(NO_STORE));
    Ok(response)
}

/// Port of `databaseRecycle` (api4/system.go:318) — `POST /api/v4/database/recycle`: the
/// gate, then this process's pool is recycled (`App::recycle_database_connection`), then
/// `{"status":"OK"}`. Go's pool is Go's to recycle; nothing here asks it to, since a pool is a
/// process's own and a client that wants Go's recycled calls Go.
#[tracing::instrument(skip_all)]
pub async fn database_recycle(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_RECYCLE_DATABASE_CONNECTIONS).await?;
    state.app.recycle_database_connection().await;
    Ok(status_ok())
}

// ------------------------------------------------------------------------------------------
// GET /api/v4/logs, POST /api/v4/logs/query, GET /api/v4/logs/download
// ------------------------------------------------------------------------------------------

/// Port of `getLogs` (api4/system.go:397) — `GET /api/v4/logs`, on both routers.
///
/// `model.ArrayToJSON(lines)`: a `json.Marshal` of a `[]string`, whose nil form — the
/// `logs_per_page=0` page — is the four bytes `null`. See `mm_app::logs` for the leading
/// newline on every line but the first and for why this stack answers a 403.
#[tracing::instrument(skip_all, fields(page, logs_per_page))]
pub async fn get_logs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_GET_LOGS).await?;

    let page = page_param(query.as_deref());
    let per_page = logs_per_page_param(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("logs_per_page", per_page);

    let lines = state.app.get_logs(page, per_page).await?;
    Ok(json_response(
        StatusCode::OK,
        array_to_json(&lines).into_bytes(),
    ))
}

/// `model.ArrayToJSON` — `json.Marshal` of a `[]string`, `null` for the nil slice Go builds
/// when no line was read.
fn array_to_json(lines: &[String]) -> String {
    if lines.is_empty() {
        return "null".to_owned();
    }
    go_json_marshal(&lines).unwrap_or_else(|_| "null".to_owned())
}

/// Port of `queryLogs` (api4/system.go:354) — `POST /api/v4/logs/query`.
///
/// # A bad body is a **500**, not a 400
///
/// `json.NewDecoder(r.Body).Decode(&logFilter)` failing — or decoding `null` — is
/// `api.system.logs.invalidFilter` at `http.StatusInternalServerError`. A non-object body (`[]`,
/// `"x"`, `5`) is a decode error; `{}` is a filter with nothing set. Then the page is
/// `App::query_logs`, keyed by node, and each line is **re-encoded**: `json.Unmarshal` into
/// `any` and `model.ToJSON` back out, so keys come back sorted, numbers as Go `float64`s and
/// strings HTML-escaped; a line that is not JSON is dropped with a warning. The gate precedes the
/// decode, so an unauthorised caller with a bad body sees the 403.
#[tracing::instrument(skip_all, fields(page, logs_per_page, nodes))]
pub async fn query_logs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_GET_LOGS).await?;

    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    // Through a `Value` first: serde's derive would read a JSON *array* as a struct, field by
    // position, where Go's decoder refuses anything but an object (or `null`, which is the nil
    // pointer this handler also refuses).
    let filter: LogFilter = match serde_json::from_slice::<Value>(&bytes) {
        Ok(object @ Value::Object(_)) => match serde_json::from_value::<LogFilter>(object) {
            Ok(filter) => filter,
            Err(_) => {
                return Err(ApiError::from(AppError::new(
                    "queryLogs",
                    "api.system.logs.invalidFilter",
                    None,
                    String::new(),
                    500,
                )));
            }
        },
        _ => {
            return Err(ApiError::from(AppError::new(
                "queryLogs",
                "api.system.logs.invalidFilter",
                None,
                String::new(),
                500,
            )));
        }
    };

    let page = page_param(query.as_deref());
    let per_page = logs_per_page_param(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("logs_per_page", per_page);

    let logs = state.app.query_logs(page, per_page, &filter).await?;
    tracing::Span::current().record("nodes", logs.len());

    let mut logs_json: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (node, lines) in &logs {
        for line in lines {
            match serde_json::from_str::<Value>(line) {
                Ok(parsed) => logs_json.entry(node.clone()).or_default().push(parsed),
                Err(err) => {
                    tracing::warn!(from_node = %node, error = %err, "Error parsing log line in Server Logs")
                }
            }
        }
    }
    Ok(json_response(
        StatusCode::OK,
        string_map_of_lists_to_json(&logs_json).into_bytes(),
    ))
}

/// `model.ToJSON(map[string][]any)` — Go's encoder over values that were themselves decoded
/// by Go: object keys sorted, every number a `float64`, strings HTML-escaped. A node that
/// contributed no parseable line is absent from the map, as `logsJSON[node] = append(…)` never
/// ran for it.
fn string_map_of_lists_to_json(map: &BTreeMap<String, Vec<Value>>) -> String {
    let mut out = String::from("{");
    for (i, (node, values)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        go_marshal_value(&Value::String(node.clone()), &mut out);
        out.push(':');
        out.push('[');
        for (j, value) in values.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            go_marshal_value(value, &mut out);
        }
        out.push(']');
    }
    out.push('}');
    out
}

/// `json.Marshal` of an `any` that `json.Unmarshal` produced: `map[string]any` with sorted
/// keys, `[]any`, `float64`, `string`, `bool`, `nil`.
fn go_marshal_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let rendered = n
                .as_f64()
                .and_then(go_json_format_float)
                .unwrap_or_else(|| n.to_string());
            out.push_str(&rendered);
        }
        Value::String(s) => {
            let quoted = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned());
            out.push_str(&go_json_escape(&quoted));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                go_marshal_value(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            // `serde_json::Map` is a `BTreeMap` in this build, so the keys are already in the
            // byte order Go sorts them into.
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                go_marshal_value(&Value::String(key.clone()), out);
                out.push(':');
                go_marshal_value(item, out);
            }
            out.push('}');
        }
    }
}

/// Port of `downloadLogs` (api4/system.go:420) — `GET /api/v4/logs/download`.
///
/// Every failure of `GetLogFile` — file logging off, the root check, the read — is one 500,
/// `api.system.logs.download_bytes_buffer.app_error`. The success is
/// `web.WriteFileResponse(config.LogFilename, "text/plain", size, time.Now(), webserverMode,
/// reader, forceDownload = true)`: an attachment named `mattermost.log`, whose `Last-Modified`
/// is the instant of the request and never the file's.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn download_logs(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_GET_LOGS).await?;

    let download_error = |err: &dyn std::fmt::Display| {
        tracing::warn!(error = %err, "the log file could not be read for download");
        ApiError::from(AppError::new(
            "downloadLogs",
            "api.system.logs.download_bytes_buffer.app_error",
            None,
            String::new(),
            500,
        ))
    };
    let path = state
        .app
        .get_log_file()
        .await
        .map_err(|err| download_error(&err))?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|err| download_error(&err))?;
    let size = file
        .metadata()
        .await
        .map_err(|err| download_error(&err))?
        .len();

    let spec = FileResponseSpec {
        filename: LOG_FILENAME,
        content_type: "text/plain",
        content_size: i64::try_from(size).unwrap_or(i64::MAX),
        modtime_millis: Some(mm_model::utils::get_millis()),
        webserver_mode: &state.app.config().webserver_mode,
        force_download: true,
    };
    let (parts, _) = request.into_parts();
    match write_file_response(&spec, &parts.method, &parts.headers, file, size).await {
        FileResponse::Response(mut response) => {
            response
                .headers_mut()
                .insert("x-mmrs-served-by", HeaderValue::from_static("rust"));
            Ok(response)
        }
        FileResponse::Forward(reason) => {
            tracing::Span::current().record("forwarded", reason);
            Ok(proxy::forward_to_go(State(state), Request::from_parts(parts, Body::empty())).await)
        }
    }
}

// ------------------------------------------------------------------------------------------
// POST /api/v4/restart
// ------------------------------------------------------------------------------------------

/// Port of `restart` (api4/system.go:979) — `POST /api/v4/restart`.
///
/// # What a restart means here
///
/// Go answers `{"status":"OK"}`, sleeps one second **inside the handler** — so the client
/// receives the 200 after the sleep, not before — and then, in a goroutine, calls
/// `Server.Restart`, which execs the binary in place *only if an enterprise upgrade has
/// completed* and otherwise does nothing (`App::restart` explains the nil-wrapped error).
/// So on every server this project runs, the observable behaviour is a one-second 200 and no
/// restart, on both sides; the exec is real and unreachable.
///
/// The decision, for the day it is reachable: **each process restarts itself and only
/// itself.** This handler never asks the Go server to restart, because a process cannot restart
/// another and because the client's view of "the server" is the front it connected to — this
/// one — which comes back on the same port with the same arguments. The Go server behind the
/// proxy is test apparatus, and restarting it from here would only take the oracle down.
#[tracing::instrument(skip_all)]
pub async fn restart(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate(&state, &session, &PERMISSION_MANAGE_SYSTEM).await?;

    // `ReturnStatusOK(w); time.Sleep(1 * time.Second)`: the body is written before the sleep
    // and flushed after it, so the client sees the 200 a second late. Then the goroutine.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let app = state.app.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(err) = app.restart() {
            tracing::error!(error = %err, "Error while restarting server");
        }
    });
    Ok(status_ok())
}

// ------------------------------------------------------------------------------------------
// POST /api/v4/upgrade_to_enterprise, GET …/status, GET …/allowed
// ------------------------------------------------------------------------------------------

/// The 403 `upgradeToEnterprise` and `isAllowedToUpgradeToEnterprise` share for an
/// architecture the upgrader does not support.
fn system_not_supported(where_: &str) -> ApiError {
    ApiError::from(AppError::new(
        where_,
        "api.upgrade_to_enterprise.system_not_supported.app_error",
        None,
        String::new(),
        403,
    ))
}

/// Port of `upgradeToEnterprise` (api4/system.go:870) — `POST /api/v4/upgrade_to_enterprise`.
///
/// The gate, then five refusals in Go's order: an enterprise-ready build (429), an upgrade in
/// progress (`percentage > 0`, 429), one finished (`== 100`, 429, unreachable behind the
/// previous check and kept in its place), then `CanIUpgradeToE0`'s three-way `switch` — the
/// permission errors at 403 with the two usernames and the directory as params, the
/// architecture at 403, anything else at 403 `generic_error`. On this host the architecture
/// refusal is the answer, and it is the same one Go gives.
///
/// # The arm that starts the upgrade is forwarded
///
/// Past every check Go spawns `UpgradeToE0`, which downloads the amd64 Team Edition tarball,
/// verifies its signature and swaps the running executable — a procedure with no meaning for
/// this binary (see `mm_app::upgrader`). That arm forwards to Go, whose own binary is the one the
/// procedure was written for; the status and restart routes then report *this* process, which
/// has not been upgraded. Reachable only on a Linux amd64 host whose executable directory the
/// process may write, which no stack in this project is ([D-682]).
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn upgrade_to_enterprise(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    gate(&state, &session, &PERMISSION_MANAGE_SYSTEM).await?;

    let refuse = |id: &str, status: i32, params| {
        ApiError::from(AppError::new(
            "upgradeToEnterprise",
            id,
            params,
            String::new(),
            status,
        ))
    };

    if BUILD_ENTERPRISE_READY == "true" {
        return Err(refuse(
            "api.upgrade_to_enterprise.already-enterprise.app_error",
            429,
            None,
        ));
    }

    let (percentage, _) = upgrade_to_e0_status();
    if percentage > 0 {
        return Err(refuse("api.upgrade_to_enterprise.app_error", 429, None));
    }
    if percentage == 100 {
        return Err(refuse(
            "api.upgrade_to_enterprise.already-done.app_error",
            429,
            None,
        ));
    }

    if let Err(cannot) = can_i_upgrade_to_e0() {
        return Err(match cannot.cause {
            UpgradeError::InvalidPermissions {
                err_type,
                path,
                file_username,
                mattermost_username,
            } => {
                let params = std::collections::HashMap::from([
                    (
                        "MattermostUsername".to_owned(),
                        Value::String(mattermost_username),
                    ),
                    ("FileUsername".to_owned(), Value::String(file_username)),
                    ("Path".to_owned(), Value::String(path)),
                ]);
                let id = match err_type {
                    INVALID_USER_AND_PERMISSION => {
                        "api.upgrade_to_enterprise.invalid-user-and-permission.app_error"
                    }
                    INVALID_USER => "api.upgrade_to_enterprise.invalid-user.app_error",
                    INVALID_PERMISSION => "api.upgrade_to_enterprise.invalid-permission.app_error",
                    // Go's `if / else if / else if` has no final `else`: an unknown type sets no
                    // error and the handler returns — an empty 200 with no body. There is no
                    // fourth type, so this arm is written to say so rather than reached.
                    _ => "api.upgrade_to_enterprise.generic_error.app_error",
                };
                refuse(id, 403, Some(params))
            }
            UpgradeError::InvalidArch => system_not_supported("upgradeToEnterprise"),
            UpgradeError::Other(_) => refuse(
                "api.upgrade_to_enterprise.generic_error.app_error",
                403,
                None,
            ),
        });
    }

    tracing::Span::current().record("forwarded", "UpgradeToE0 is the Go binary's procedure");
    Ok(proxy::forward_to_go(State(state), request).await)
}

/// Port of `upgradeToEnterpriseStatus` (api4/system.go:932) — `GET
/// /api/v4/upgrade_to_enterprise/status`: the gate, then `{"error":null,"percentage":N}` from
/// `model.StringInterfaceToJSON`, keys sorted. An upgrader error would be the *message* of a
/// 400 `AppError` as the `error` string with `percentage` 0 — and no upgrader runs here, so the
/// answer is the zero form.
#[tracing::instrument(skip_all)]
pub async fn upgrade_to_enterprise_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate(&state, &session, &PERMISSION_MANAGE_SYSTEM).await?;

    let (percentage, error) = upgrade_to_e0_status();
    let body = match error {
        Some(message) => {
            // Both of Go's arms build an `AppError` whose `Message` is the id itself until
            // translation, and this server does not translate ([D-092]).
            let _ = message;
            let mut out = String::from("{\"error\":");
            go_marshal_value(
                &Value::String("api.upgrade_to_enterprise_status.signature.app_error".to_owned()),
                &mut out,
            );
            out.push_str(",\"percentage\":0}");
            out
        }
        None => format!("{{\"error\":null,\"percentage\":{percentage}}}"),
    };
    Ok(json_response(StatusCode::OK, body.into_bytes()))
}

/// Port of `isAllowedToUpgradeToEnterprise` (api4/system.go:959) — `GET
/// /api/v4/upgrade_to_enterprise/allowed`: the gate, then `CanIUpgradeToE0` with **two**
/// outcomes rather than the POST's four — the architecture is
/// `system_not_supported`, and *any other* error becomes a 403 whose id is the error's own
/// rendered message (`err.Error()` passed where an id goes), permissions included.
#[tracing::instrument(skip_all)]
pub async fn is_allowed_to_upgrade_to_enterprise(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate(&state, &session, &PERMISSION_MANAGE_SYSTEM).await?;

    match can_i_upgrade_to_e0() {
        Ok(()) => Ok(status_ok()),
        Err(cannot) if cannot.cause == UpgradeError::InvalidArch => {
            Err(system_not_supported("isAllowedToUpgradeToEnterprise"))
        }
        Err(cannot) => Err(ApiError::from(AppError::new(
            "isAllowedToUpgradeToEnterprise",
            cannot.message(),
            None,
            String::new(),
            403,
        ))),
    }
}

// ------------------------------------------------------------------------------------------
// POST /api/v4/elasticsearch/test, POST /api/v4/elasticsearch/purge_indexes
// ------------------------------------------------------------------------------------------

/// The `json:` names of every pointer field of `model.ElasticsearchSettings`
/// (config.go:3233) **except `BulkIndexingTimeWindowSeconds`**, which `testElasticsearch`
/// fills in before the nil check "to avoid failing on the nil check". Twenty-nine fields;
/// `checkHasNilFields` refuses the request if any is nil.
const ELASTICSEARCH_CHECKED_FIELDS: [&str; 29] = [
    "ConnectionURL",
    "Backend",
    "Username",
    "Password",
    "EnableIndexing",
    "EnableSearching",
    "EnableCJKAnalyzers",
    "EnableAutocomplete",
    "Sniff",
    "PostIndexReplicas",
    "PostIndexShards",
    "ChannelIndexReplicas",
    "ChannelIndexShards",
    "UserIndexReplicas",
    "UserIndexShards",
    "AggregatePostsAfterDays",
    "PostsAggregatorJobStartTime",
    "IndexPrefix",
    "GlobalSearchPrefix",
    "LiveIndexingBatchSize",
    "BatchSize",
    "RequestTimeoutSeconds",
    "SkipTLSVerification",
    "CA",
    "ClientCert",
    "ClientKey",
    "Trace",
    "IgnoredPurgeIndexes",
    "EnableSearchPublicChannelsWithoutMembership",
];

/// `encoding/json`'s field lookup for an untagged struct field: the exact key first, then the
/// first key that matches case-insensitively.
fn go_field<'a>(object: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.get(name).or_else(|| {
        object
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}

/// What `json.NewDecoder(r.Body).Decode(&cfg)` leaves in `cfg.ElasticsearchSettings`, judged
/// by `checkHasNilFields` and read by `TestElasticsearch`.
///
/// `Ok(None)` is a nil `cfg` — a body that is `null` or not JSON at all — for which Go
/// substitutes the running configuration. `Ok(Some(_))` is a non-nil `cfg` whose settings all
/// passed the nil check, with the three values the test reads; `Err(())` is a nil field.
///
/// The decoder's rules, measured against Go on 2026-09-15: a body that is JSON but not an
/// object (`[]`, `5`, `"x"`) allocates a zero `Config` and is all-nil; a missing or `null`
/// section is all-nil; a section that is not an object is all-nil; a field that is **present
/// with any non-null value** is non-nil, because the decoder allocates the pointer before it
/// discovers a type mismatch — so `"ConnectionURL": 5` passes the nil check as `""`, and only
/// an absent or `null` field fails it. Keys match case-insensitively.
fn decode_elasticsearch_body(bytes: &[u8]) -> Result<Option<ElasticsearchTestSettings>, ()> {
    let body: Value = match serde_json::from_slice(bytes) {
        Ok(Value::Null) | Err(_) => return Ok(None),
        Ok(body) => body,
    };
    let Some(config) = body.as_object() else {
        return Err(());
    };
    let Some(section) = go_field(config, "ElasticsearchSettings").and_then(Value::as_object) else {
        return Err(());
    };
    for field in ELASTICSEARCH_CHECKED_FIELDS {
        match go_field(section, field) {
            None | Some(Value::Null) => return Err(()),
            Some(_) => {}
        }
    }
    let string_of = |name: &str| -> String {
        match go_field(section, name) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    };
    Ok(Some(ElasticsearchTestSettings {
        connection_url: string_of("ConnectionURL"),
        username: string_of("Username"),
        password: string_of("Password"),
    }))
}

/// Port of `testElasticsearch` (api4/elasticsearch.go:19) — `POST /api/v4/elasticsearch/test`.
///
/// # The body is validated **before** the permission check
///
/// Decode, patch `BulkIndexingTimeWindowSeconds`, refuse a nil field (400
/// `test_elasticsearch_settings_nil`) — and only then `SessionHasPermissionToAndNotRestrictedAdmin(
/// test_elasticsearch)`. So a caller with no rights learns whether their body was complete. Then
/// `App::test_elasticsearch`: the re-enter-password 400, or the 501 this build always ends in.
#[tracing::instrument(skip_all, fields(body_used))]
pub async fn test_elasticsearch(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    let running = mm_app::config::load_model_config(state.app.store().config())
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "could not load the running configuration");
            ApiError::from(AppError::new(
                "testElasticsearch",
                "api.config.get_config.app_error",
                None,
                String::new(),
                500,
            ))
        })?;

    let mut cfg = match decode_elasticsearch_body(&bytes) {
        Ok(Some(cfg)) => {
            tracing::Span::current().record("body_used", true);
            cfg
        }
        Ok(None) => {
            tracing::Span::current().record("body_used", false);
            ElasticsearchTestSettings::from_running(&running)
        }
        Err(()) => {
            return Err(ApiError::from(AppError::new(
                "testElasticsearch",
                "api.elasticsearch.test_elasticsearch_settings_nil.app_error",
                None,
                String::new(),
                400,
            )));
        }
    };

    gate_not_restricted(&state, &session, &PERMISSION_TEST_ELASTICSEARCH).await?;

    state.app.test_elasticsearch(&mut cfg, &running)?;
    Ok(status_ok())
}

/// Port of `purgeElasticsearchIndexes` (api4/elasticsearch.go:57) — `POST
/// /api/v4/elasticsearch/purge_indexes`: the gate, then `?index=` (every value) to
/// `App::purge_elasticsearch_indexes`, which is the 501 on this build.
#[tracing::instrument(skip_all, fields(indexes))]
pub async fn purge_elasticsearch_indexes(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    gate_not_restricted(&state, &session, &PERMISSION_PURGE_ELASTICSEARCH_INDEXES).await?;
    let indexes = query_get_all(query.as_deref(), "index");
    tracing::Span::current().record("indexes", indexes.len());
    state.app.purge_elasticsearch_indexes(&indexes)?;
    Ok(status_ok())
}

// ------------------------------------------------------------------------------------------
// local mode: POST /api/v4/integrity
// ------------------------------------------------------------------------------------------

/// Port of `localCheckIntegrity` (api4/system_local.go:25) — `POST /api/v4/integrity` on the
/// unix socket only. No gate: the socket is the credential. Every check is drained and the
/// list is `json.Marshal`ed, no trailing newline.
#[tracing::instrument(skip_all, fields(checks))]
pub async fn local_check_integrity(State(state): State<AppState>) -> Result<Response, ApiError> {
    let results = state.app.check_integrity().await;
    tracing::Span::current().record("checks", results.len());
    let body = go_json_marshal(&results).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the integrity results");
        ApiError::from(AppError::new(
            "Api4.localCheckIntegrity",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    Ok(json_response(StatusCode::OK, body.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `page`: a failure or a negative is zero. `logs_per_page`: a failure or a negative is
    /// 10,000, over 10,000 is 10,000, and zero is zero.
    #[test]
    fn the_paging_params_clamp_like_web_params() {
        assert_eq!(page_param(Some("page=3")), 3);
        assert_eq!(page_param(Some("page=-1")), 0);
        assert_eq!(page_param(Some("page=x")), 0);
        assert_eq!(page_param(None), 0);
        assert_eq!(logs_per_page_param(Some("logs_per_page=25")), 25);
        assert_eq!(logs_per_page_param(Some("logs_per_page=0")), 0);
        assert_eq!(logs_per_page_param(Some("logs_per_page=-5")), 10_000);
        assert_eq!(logs_per_page_param(Some("logs_per_page=10001")), 10_000);
        assert_eq!(logs_per_page_param(Some("logs_per_page=abc")), 10_000);
        assert_eq!(logs_per_page_param(None), 10_000);
    }

    /// `ArrayToJSON` of nil is `null`; of lines it is the escaped array, leading newlines and
    /// angle brackets included.
    #[test]
    fn array_to_json_is_gos() {
        assert_eq!(array_to_json(&[]), "null");
        assert_eq!(
            array_to_json(&["a".to_owned(), "\n<b>".to_owned()]),
            r#"["a","\n\u003cb\u003e"]"#
        );
    }

    /// The re-encoded log map: sorted keys, `float64` numbers, HTML escapes, and a node with
    /// no lines absent.
    #[test]
    fn the_query_response_is_gos_re_encoding() {
        let mut map = BTreeMap::new();
        map.insert(
            "default".to_owned(),
            vec![
                serde_json::from_str(
                    r#"{"z":1,"a":"<x>","n":1000000,"f":0.5,"e":1e-7,"b":[true,null]}"#,
                )
                .unwrap(),
            ],
        );
        assert_eq!(
            string_map_of_lists_to_json(&map),
            r#"{"default":[{"a":"\u003cx\u003e","b":[true,null],"e":1e-7,"f":0.5,"n":1000000,"z":1}]}"#
        );
        assert_eq!(string_map_of_lists_to_json(&BTreeMap::new()), "{}");
    }

    /// The decoder's verdicts, one per measured Go answer: nothing / `null` is the running
    /// configuration; `{}`, `[]`, `5`, `"x"`, a missing section and a non-object section are
    /// nil fields; a complete section passes; a wrong-typed value is present and empty; keys
    /// match case-insensitively; one absent field fails.
    #[test]
    fn the_body_decodes_like_encoding_json_into_a_config_pointer() {
        let full = |password: &str| {
            let mut section = serde_json::Map::new();
            for field in ELASTICSEARCH_CHECKED_FIELDS {
                section.insert(field.to_owned(), Value::Bool(true));
            }
            section.insert(
                "ConnectionURL".to_owned(),
                Value::String("http://es".into()),
            );
            section.insert("Username".to_owned(), Value::String("elastic".into()));
            section.insert("Password".to_owned(), Value::String(password.into()));
            section
        };
        let body = |section: serde_json::Map<String, Value>| {
            serde_json::to_vec(&serde_json::json!({ "ElasticsearchSettings": section })).unwrap()
        };

        assert_eq!(decode_elasticsearch_body(b""), Ok(None));
        assert_eq!(decode_elasticsearch_body(b"null"), Ok(None));
        assert_eq!(decode_elasticsearch_body(b"{bad"), Ok(None));
        for nil in [
            &b"{}"[..],
            b"[]",
            b"5",
            b"\"x\"",
            br#"{"ElasticsearchSettings":{}}"#,
            br#"{"ElasticsearchSettings":5}"#,
            br#"{"ElasticsearchSettings":null}"#,
        ] {
            assert_eq!(
                decode_elasticsearch_body(nil),
                Err(()),
                "{}",
                String::from_utf8_lossy(nil)
            );
        }

        let complete = decode_elasticsearch_body(&body(full("pw")))
            .unwrap()
            .unwrap();
        assert_eq!(
            complete,
            ElasticsearchTestSettings {
                connection_url: "http://es".into(),
                username: "elastic".into(),
                password: "pw".into(),
            }
        );

        // A wrong type is allocated, not nil: present as the zero string.
        let mut typed = full("pw");
        typed.insert("ConnectionURL".to_owned(), Value::Number(5.into()));
        let decoded = decode_elasticsearch_body(&body(typed)).unwrap().unwrap();
        assert_eq!(decoded.connection_url, "");

        // Case-insensitive keys.
        let lower: serde_json::Map<String, Value> = full("pw")
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let bytes =
            serde_json::to_vec(&serde_json::json!({ "elasticsearchsettings": lower })).unwrap();
        assert!(decode_elasticsearch_body(&bytes).unwrap().is_some());

        // One field missing, or null, is nil.
        let mut missing = full("pw");
        missing.remove("Sniff");
        assert_eq!(decode_elasticsearch_body(&body(missing)), Err(()));
        let mut nulled = full("pw");
        nulled.insert("Trace".to_owned(), Value::Null);
        assert_eq!(decode_elasticsearch_body(&body(nulled)), Err(()));
        // The patched field is not required.
        let mut no_bulk = full("pw");
        no_bulk.remove("BulkIndexingTimeWindowSeconds");
        assert!(decode_elasticsearch_body(&body(no_bulk)).unwrap().is_some());
    }

    /// The four `ErrType`s and the architecture map to their ids; the wrapped message is the
    /// `/allowed` id for anything but the architecture.
    #[test]
    fn the_upgrade_refusals_carry_gos_ids() {
        assert_eq!(
            system_not_supported("x").0.id,
            "api.upgrade_to_enterprise.system_not_supported.app_error"
        );
        assert_eq!(system_not_supported("x").0.status_code, 403);
        assert_eq!(ELASTICSEARCH_CHECKED_FIELDS.len(), 29);
    }
}
