//! Port of `api4/scheme.go` — all seven `/api/v4/schemes` routes.
//!
//! # Four reads and three refusals
//!
//! The four GETs are ordinary paged reads over `Schemes`, `Teams` and `Channels`. The three
//! writes — create, patch, delete — each begin with the **same** licence test and answer **501**
//! on an unlicensed server, before any permission check and before touching the database. On the
//! deployment this server runs beside, that 501 is the entire route: there is no reachable path
//! past it, so the three are ported completely rather than partially. A licensed installation is
//! forwarded, because the test's second half reads `Features.CustomPermissionsSchemes` and
//! `SkuShortName` out of the signed licence body, which is not ported.
//!
//! # The order of the refusals is load-bearing and differs per route
//!
//! `createScheme` decodes the body **first** — so a malformed body is a 400 even on an unlicensed
//! server, and only a well-formed one reaches the 501. `patchScheme` validates the id, then
//! decodes, then tests the licence. `deleteScheme` validates the id and goes straight to the
//! licence. Three routes, three orders, and each is observable.

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_PERMISSIONS,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_TEAMS, make_permission_error,
};
use mm_model::scheme::{SCHEME_SCOPE_CHANNEL, SCHEME_SCOPE_TEAM};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, require_id};
use crate::error::ApiError;
use crate::proxy;

/// `?scope=`, the only query parameter `getSchemes` reads beyond paging.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ScopeParam {
    scope: Option<String>,
}

/// Port of `getSchemes` (scheme.go:82).
///
/// # The scope is validated against two literals, and an **empty** scope is valid
///
/// `if scope != "" && scope != team && scope != channel { SetInvalidParam("scope") }` — so an
/// absent or empty `?scope=` means "every scope" and anything else is a 400. `playbook` and `run`
/// are real `SchemeScope` values (`model/scheme.go`) and are **not** accepted here; a port that
/// validated against `Scheme::is_valid`'s list would admit two scopes Go rejects.
///
/// `json.Marshal` and a bare `Write`: no trailing newline.
#[tracing::instrument(skip_all, fields(scope, page, per_page, count))]
pub async fn get_schemes(
    State(state): State<AppState>,
    Query(params): Query<ScopeParam>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_PERMISSIONS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_PERMISSIONS],
        )));
    }

    let scope = params.scope.unwrap_or_default();
    if !scope_is_accepted(&scope) {
        return Err(ApiError::invalid_param("scope"));
    }
    tracing::Span::current().record("scope", &scope);

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let schemes = state.app.get_schemes_page(&scope, page, per_page).await?;
    tracing::Span::current().record("count", schemes.len());

    encode("getSchemes", &schemes, Newline::No)
}

/// Go's scope test (scheme.go:88), as a value.
///
/// Three accepted inputs, and the empty string is one of them.
fn scope_is_accepted(scope: &str) -> bool {
    scope.is_empty() || scope == SCHEME_SCOPE_TEAM || scope == SCHEME_SCOPE_CHANNEL
}

/// Port of `getScheme` (scheme.go:62).
///
/// `json.NewEncoder(w).Encode`, so a **trailing newline** — unlike `getSchemes` immediately above
/// it in the same Go file, which uses `json.Marshal`.
#[tracing::instrument(skip_all, fields(scheme_id = %scheme_id))]
pub async fn get_scheme(
    State(state): State<AppState>,
    Path(scheme_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&scheme_id, "scheme_id")?;

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_PERMISSIONS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_PERMISSIONS],
        )));
    }

    let scheme = state.app.get_scheme(&scheme_id).await?;
    encode("getScheme", &scheme, Newline::Yes)
}

/// Port of `getTeamsForScheme` (scheme.go:110).
///
/// # The scope check is a **400**, not a 404 or an empty list
///
/// A channel-scoped scheme asked for its teams answers
/// `api.scheme.get_teams_for_scheme.scope.error` at 400 — the scheme exists, the question does
/// not apply. That branch is reached only after the scheme has been loaded, so a missing id is
/// still the app layer's 404.
///
/// # `SanitizeTeams` runs on the way out
///
/// Each team is stripped of `email` and `allowed_domains` unless the session can manage it —
/// `sanitize_team`, the same rule every other team listing applies.
///
/// `json.Marshal` and a bare `Write`: no trailing newline.
#[tracing::instrument(skip_all, fields(scheme_id = %scheme_id, page, per_page, count))]
pub async fn get_teams_for_scheme(
    State(state): State<AppState>,
    Path(scheme_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&scheme_id, "scheme_id")?;

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_TEAMS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_TEAMS],
        )));
    }

    let scheme = state.app.get_scheme(&scheme_id).await?;
    if scheme.scope != SCHEME_SCOPE_TEAM {
        return Err(ApiError::from(AppError::new(
            "Api4.GetTeamsForScheme",
            "api.scheme.get_teams_for_scheme.scope.error",
            None,
            String::new(),
            400,
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let mut teams = state
        .app
        .get_teams_for_scheme_page(&scheme_id, page, per_page)
        .await?;
    tracing::Span::current().record("count", teams.len());
    state.app.sanitize_teams(&session.0, &mut teams).await;

    encode("getTeamsForScheme", &teams, Newline::No)
}

/// Port of `getChannelsForScheme` (scheme.go:148).
///
/// The mirror of [`get_teams_for_scheme`], with three differences that all matter: the permission
/// is the *channels* sysconsole read, the scope must be `channel`, and the writer is
/// `json.NewEncoder(w).Encode` — so this one **does** end in a newline where its twin does not.
/// There is no sanitization step; `model.Channel` has nothing to strip.
#[tracing::instrument(skip_all, fields(scheme_id = %scheme_id, page, per_page, count))]
pub async fn get_channels_for_scheme(
    State(state): State<AppState>,
    Path(scheme_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&scheme_id, "scheme_id")?;

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS,
        )
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS],
        )));
    }

    let scheme = state.app.get_scheme(&scheme_id).await?;
    if scheme.scope != SCHEME_SCOPE_CHANNEL {
        return Err(ApiError::from(AppError::new(
            "Api4.GetChannelsForScheme",
            "api.scheme.get_channels_for_scheme.scope.error",
            None,
            String::new(),
            400,
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let channels = state
        .app
        .get_channels_for_scheme_page(&scheme_id, page, per_page)
        .await?;
    tracing::Span::current().record("count", channels.len());

    encode("getChannelsForScheme", &channels, Newline::Yes)
}

/// Port of `createScheme` (scheme.go:25).
///
/// # The body is decoded before the licence is consulted
///
/// So `POST /api/v4/schemes` with `{` is a **400** on an unlicensed server and a well-formed body
/// is a 501. Reversing the two would answer 501 to a malformed body, which is a different
/// contract for every client that retries on 400.
///
/// The permission check comes *after* the licence test, so an unlicensed server never reports
/// `sysconsole_write_user_management_permissions` — a non-admin gets the same 501 an admin does.
/// That is why this file does not import that permission at all: on this deployment it is
/// unreachable, and importing it would suggest a check that does not run.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_scheme(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "the request body could not be read");
            return ApiError::invalid_param("scheme").into_response();
        }
    };

    // `json.NewDecoder(r.Body).Decode(&scheme)` — a `model.Scheme` with every field optional, so
    // `{}` decodes and only malformed JSON fails.
    if serde_json::from_slice::<mm_model::scheme::Scheme>(&bytes).is_err() {
        return ApiError::invalid_param("scheme").into_response();
    }

    licence_gate(
        &state,
        "Api4.CreateScheme",
        "api.scheme.create_scheme.license.error",
        parts,
        bytes,
    )
    .await
}

/// Port of `patchScheme` (scheme.go:178).
///
/// Id, then body, then licence. The body is a `model.SchemePatch`, which is all pointers, so `{}`
/// is well-formed.
#[tracing::instrument(skip_all, fields(scheme_id, licensed))]
pub async fn patch_scheme(
    State(state): State<AppState>,
    Path(scheme_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("scheme_id", &scheme_id);
    if let Err(err) = require_id(&scheme_id, "scheme_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "the request body could not be read");
            return ApiError::invalid_param("scheme").into_response();
        }
    };
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        // `SetInvalidParamWithErr("scheme", …)` — note the parameter is `scheme`, not
        // `scheme_patch`, even though the type is `model.SchemePatch`.
        return ApiError::invalid_param("scheme").into_response();
    }

    licence_gate(
        &state,
        "Api4.PatchScheme",
        "api.scheme.patch_scheme.license.error",
        parts,
        bytes,
    )
    .await
}

/// Port of `deleteScheme` (scheme.go:229).
///
/// Id, then licence. No body, so nothing between them.
#[tracing::instrument(skip_all, fields(scheme_id, licensed))]
pub async fn delete_scheme(
    State(state): State<AppState>,
    Path(scheme_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("scheme_id", &scheme_id);
    if let Err(err) = require_id(&scheme_id, "scheme_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => axum::body::Bytes::new(),
    };
    licence_gate(
        &state,
        "Api4.DeleteScheme",
        "api.scheme.delete_scheme.license.error",
        parts,
        bytes,
    )
    .await
}

/// The licence test the three write routes share (scheme.go:36, :193, :239), verbatim in all
/// three: `License() == nil || (!*Features.CustomPermissionsSchemes && SkuShortName !=
/// LicenseShortSkuProfessional)`.
///
/// Unlicensed answers 501 with the route's own id. Licensed forwards, because the remaining two
/// clauses read the signed licence body.
async fn licence_gate(
    state: &AppState,
    where_: &'static str,
    id: &'static str,
    parts: axum::http::request::Parts,
    body: axum::body::Bytes,
) -> Response {
    let licensed = match state.app.license_state().await {
        Ok(state) => state == mm_app::license::LicenseState::Licensed,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("licensed", licensed);

    if licensed {
        let request = Request::from_parts(parts, axum::body::Body::from(body));
        return proxy::forward_to_go(State(state.clone()), request).await;
    }

    ApiError::from(AppError::new(where_, id, None, String::new(), 501)).into_response()
}

/// Whether the encoder writes a trailing newline. `json.NewEncoder(w).Encode` does;
/// `json.Marshal` plus `w.Write` does not, and this file uses both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Newline {
    Yes,
    No,
}

/// The response body bytes, newline included or not.
///
/// Split from [`encode`] because a `Response`'s body is a stream: `assert_ne!` on two
/// `Response`s compares their **debug** output, which shows `Body(UnsyncBoxBody)` for both and
/// passes whatever the bytes are. The first version of the newline test did exactly that and
/// asserted nothing at all.
fn encoded_body<T: serde::Serialize>(
    where_: &'static str,
    value: &T,
    newline: Newline,
) -> Result<Vec<u8>, ApiError> {
    let mut body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the scheme payload");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    if newline == Newline::Yes {
        body.push(b'\n');
    }
    Ok(body)
}

fn encode<T: serde::Serialize>(
    where_: &'static str,
    value: &T,
    newline: Newline,
) -> Result<Response, ApiError> {
    let body = encoded_body(where_, value, newline)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope whitelist, including the two `SchemeScope` values Go's handler does **not**
    /// accept even though `Scheme::is_valid` does.
    #[test]
    fn only_the_empty_string_team_and_channel_are_accepted_scopes() {
        assert!(scope_is_accepted(""), "an absent scope means every scope");
        assert!(scope_is_accepted("team"));
        assert!(scope_is_accepted("channel"));
        assert!(
            !scope_is_accepted("playbook"),
            "a real SchemeScope the handler still refuses"
        );
        assert!(!scope_is_accepted("run"));
        assert!(!scope_is_accepted("Team"), "the comparison is exact");
        assert!(!scope_is_accepted(" "));
    }

    /// The two writers this file uses, on the **bytes**.
    ///
    /// `getSchemes` and `getScheme` sit twenty lines apart in Go and disagree about the trailing
    /// newline; so do `getTeamsForScheme` and `getChannelsForScheme`. The routes' own choices are
    /// asserted end to end by the parity suite — this pins the mechanism they choose between.
    #[test]
    fn the_newline_is_one_byte_and_it_is_the_last_one() {
        let value = serde_json::json!({"id": "x"});
        let with = encoded_body("t", &value, Newline::Yes).expect("encodes");
        let without = encoded_body("t", &value, Newline::No).expect("encodes");

        assert_eq!(without, br#"{"id":"x"}"#);
        assert_eq!(with, b"{\"id\":\"x\"}\n");
        assert_eq!(
            with.len(),
            without.len() + 1,
            "exactly one byte, and nothing else moves"
        );
        assert!(!without.ends_with(b"\n"));
    }
}
