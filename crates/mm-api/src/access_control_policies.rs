//! The sixteen access-control-policy routes of `api4/access_control.go` (`InitAccessControlPolicy`),
//! on a build whose access-control service is **nil**.
//!
//! The routes are registered unconditionally (api4/api.go:408) — there is no licence or setting
//! at registration — so every one of them answers, and what it answers is decided by the order
//! of the checks that come *before* the service is asked. That order is the port: request
//! decoding, the feature flags, the caller's permissions (system, delegated team, delegated
//! channel — each with its own store reads), the parameter validations, and then the app
//! function, which on this build is the nil-service 501 named after it
//! ([`mm_app::App::get_access_control_policy`] and its siblings). Two routes never reach the
//! service at all and are served in full: `getFieldsAutocomplete` (the property group and its
//! fields) and `searchChannelsForAccessControlPolicy` (a real `SearchAllChannels`). Two more are
//! a **200** on this build when the body names no resources: `assign` and `unassign`, whose only
//! remaining work is `ReconcilePolicyTeamScope`, a store-only reconcile that does run.
//!
//! # What a reader gets wrong
//!
//! - **A team admin's 403 and the service's 501 are decided by the store.** `ValidateTeamAdmin
//!   PolicyOwnership` searches `AccessControlPolicies` for a parent policy scoped to the team
//!   (or whose child channels are all in it) — a row that *can* exist on this build, since the
//!   table is shared — so a team admin with an owned policy gets past the ownership gate to the
//!   501 where an unowned one is refused. The parity suite plants such rows.
//! - **The delegated permission check is a 501, not a 404.** `ValidateAccessControlPolicy
//!   Permission` reads the policy through the service, and only a 404 falls back to the channel
//!   permission; the 501 does not, so a channel admin asking for their own channel's policy is
//!   refused (`api.context.permissions.app_error`) rather than shown the service's answer.
//! - **`PUT /activate` with no entries skips the permission loop** and reaches the 501 as
//!   anyone; `assign`/`unassign` with `team_ids` are the feature-disabled 501 before any
//!   permission is consulted, for everyone, licensed or not on this stack.
//! - **`GET /{id}/activate` refuses a cookie session** with a 401 before its permission check,
//!   and checks the permission before validating `active`.
//! - **`ValidateTeamAdminSelfInclusion` turns the 501 into a 500** for a team admin saving a
//!   team policy with a non-empty rule expression.
//!
//! Every app function this module calls is a port of the Go function of the same name; see
//! `mm_app::access_control_policy` for which are constants on this build and which read the
//! store.

use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mm_model::access_policy::{
    ACCESS_CONTROL_POLICY_SCOPE_TEAM, ACCESS_CONTROL_POLICY_TYPE_CHANNEL,
    ACCESS_CONTROL_POLICY_TYPE_PARENT, ACCESS_CONTROL_POLICY_TYPE_PERMISSION,
    ACCESS_CONTROL_POLICY_TYPE_TEAM, AccessControlPolicy, AccessControlPolicyActiveUpdateRequest,
    AccessControlPolicySearch,
};
use mm_model::access_request::{
    POLICY_EVALUATION_SCOPE_ALL, POLICY_EVALUATION_SCOPE_THIS_RULE, PolicySimulationByUsersParams,
    QueryExpressionParams,
};
use mm_model::channel::{ChannelSearchOpts, ChannelsWithCount};
use mm_model::permission::{
    PERMISSION_MANAGE_CHANNEL_ACCESS_RULES, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_MANAGE_TEAM_ACCESS_RULES, Permission, make_permission_error,
};
use mm_model::utils::{AppError, decode_one_from_json, go_json_marshal, is_valid_id};
use serde::Deserialize;

use crate::AppState;
use crate::auth::{AuthenticatedSession, TokenLocation, parse_auth_token};
use crate::channels::{decode_full_channel_search, query_first, require_id};
use crate::error::ApiError;

// ---------------------------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------------------------

/// A JSON body written with `w.Write(json.Marshal(...))`: no trailing newline.
fn marshalled(where_: &'static str, value: &impl serde::Serialize) -> Response {
    match go_json_marshal(value) {
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
            tracing::error!(error = %err, "failed to serialise the response");
            ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `ReturnStatusOK` (web/web.go:127) — `model.MapToJSON`, written with `w.Write`, so no
/// trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// `c.SetPermissionError(permission)`, as the error the gates hand back.
fn permission_refusal(session: &AuthenticatedSession, permission: &Permission) -> ApiError {
    ApiError::from(make_permission_error(&session.0, &[permission]))
}

/// `c.SetPermissionError(permission)`, written.
fn permission_error(session: &AuthenticatedSession, permission: &Permission) -> Response {
    permission_refusal(session, permission).into_response()
}

/// `model.NewAppError(where, id, nil, "", http.StatusNotImplemented)` — the handler-level
/// feature-flag refusals.
fn feature_disabled_refusal(where_: &'static str, id: &'static str) -> ApiError {
    ApiError::from(AppError::new(where_, id, None, String::new(), 501))
}

fn feature_disabled(where_: &'static str, id: &'static str) -> Response {
    feature_disabled_refusal(where_, id).into_response()
}

/// The request body, or `SetInvalidParamWithErr(name, err)` when it cannot be read.
async fn body_bytes(request: Request, name: &str) -> Result<axum::body::Bytes, ApiError> {
    axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::debug!(error = %err, "could not read the request body");
            ApiError::invalid_param(name)
        })
}

/// `json.NewDecoder(r.Body).Decode(&value)` into a **struct**: one value, trailing bytes
/// ignored, and a `null` body decodes to the zero value without an error.
fn decode_struct<T: Default + serde::de::DeserializeOwned>(
    bytes: &[u8],
    name: &str,
) -> Result<T, ApiError> {
    decode_one_from_json::<Option<T>>(bytes)
        .map(Option::unwrap_or_default)
        .map_err(|err| {
            tracing::debug!(error = %err, parameter = name, "the request body did not decode");
            ApiError::invalid_param(name)
        })
}

/// `Decode(&props)` into a **pointer** followed by `props == nil`: a `null` body is the 400 too.
fn decode_pointer<T: serde::de::DeserializeOwned>(bytes: &[u8], name: &str) -> Result<T, ApiError> {
    decode_one_from_json::<Option<T>>(bytes)
        .map_err(|err| {
            tracing::debug!(error = %err, parameter = name, "the request body did not decode");
            ApiError::invalid_param(name)
        })?
        .ok_or_else(|| ApiError::invalid_param(name))
}

/// `strconv.Atoi`: an optional sign and decimal digits, nothing else; the range is the platform
/// `int`, which is 64 bits here as on the reference host.
fn atoi(raw: &str) -> Option<i64> {
    raw.parse::<i64>().ok()
}

/// Port of `teamAdminCELContextOK` (api4/access_control.go:495): a valid `team_id` the session
/// holds `manage_team_access_rules` in, and — when a channel is named — a valid channel that
/// resolves and belongs to that team. A channel that does not resolve is a plain `false`.
async fn team_admin_cel_context_ok(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
    team_id: &str,
) -> bool {
    if team_id.is_empty() || !is_valid_id(team_id) {
        return false;
    }
    if !state
        .app
        .session_has_permission_to_team(&session.0, team_id, &PERMISSION_MANAGE_TEAM_ACCESS_RULES)
        .await
    {
        return false;
    }
    if channel_id.is_empty() {
        return true;
    }
    if !is_valid_id(channel_id) {
        return false;
    }
    match state.app.get_channel(channel_id).await {
        Ok(channel) => channel.team_id == team_id,
        Err(_) => false,
    }
}

/// Who may use the CEL tooling — `checkExpression`, `validateExpressionAgainstRequester`,
/// `getFieldsAutocomplete`, `convertToVisualAST` and `authorizeSimulatePolicy` all gate the same
/// way: a system admin; else the delegated team-admin context; else a channel admin, which
/// needs a `channelId` (its absence is refused as `manage_system`) and
/// `manage_channel_access_rules` in it.
///
/// `Ok(true)` is the system admin, `Ok(false)` a delegated admin who passed.
async fn cel_tooling_gate(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
    team_id: &str,
) -> Result<bool, ApiError> {
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Ok(true);
    }
    if team_admin_cel_context_ok(state, session, channel_id, team_id).await {
        return Ok(false);
    }
    if channel_id.is_empty() {
        return Err(permission_refusal(session, &PERMISSION_MANAGE_SYSTEM));
    }
    let (ok, _) = state
        .app
        .has_permission_to_channel(
            &session.0.user_id,
            channel_id,
            &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES,
        )
        .await;
    if !ok {
        return Err(permission_refusal(
            session,
            &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES,
        ));
    }
    Ok(false)
}

/// The team-admin rung the read routes share (`getAccessControlPolicy`,
/// `getChannelsForAccessControlPolicy`, `searchChannelsForAccessControlPolicy`): a valid
/// `team_id` query parameter the session manages access rules in, and a policy the team owns —
/// otherwise `manage_system` when the team rung does not apply, `manage_team_access_rules` when
/// the policy is not the team's.
///
/// `Ok(Some(team_id))` is the authorised team.
async fn team_admin_owning(
    state: &AppState,
    session: &AuthenticatedSession,
    team_id: Option<String>,
    policy_id: &str,
) -> Result<String, ApiError> {
    let team_id = team_id.unwrap_or_default();
    if !team_id.is_empty()
        && is_valid_id(&team_id)
        && state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await
    {
        match state
            .app
            .validate_team_admin_policy_ownership(&team_id, policy_id)
            .await
        {
            Ok(true) => Ok(team_id),
            Ok(false) => Err(permission_refusal(
                session,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )),
            Err(err) => Err(ApiError::from(err)),
        }
    } else {
        Err(permission_refusal(session, &PERMISSION_MANAGE_SYSTEM))
    }
}

/// The shape `checkExpression`, `validateExpressionAgainstRequester` and `convertToVisualAST`
/// decode.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExpressionRequest {
    expression: String,
    #[serde(rename = "channelId")]
    channel_id: String,
    #[serde(rename = "teamId")]
    team_id: String,
}

/// The body of `assignAccessPolicy` and `unassignAccessPolicy`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Assignments {
    channel_ids: Option<Vec<String>>,
    team_id: String,
    team_ids: Option<Vec<String>>,
}

/// Port of `validateTeamIdsExist` (api4/access_control.go:456): every id well formed and
/// every one found, else the 400 naming `team_ids`; `GetTeams`' 404 is that 400 too.
async fn validate_team_ids_exist(state: &AppState, team_ids: &[String]) -> Result<(), ApiError> {
    if team_ids.is_empty() {
        return Ok(());
    }
    let invalid = || {
        let params = std::collections::HashMap::from([(
            "Name".to_owned(),
            serde_json::Value::String("team_ids".to_owned()),
        )]);
        ApiError::from(AppError::new(
            "validateTeamIdsExist",
            "api.context.invalid_body_param.app_error",
            Some(params),
            String::new(),
            400,
        ))
    };
    if team_ids.iter().any(|id| !is_valid_id(id)) {
        return Err(invalid());
    }
    let teams = match state.app.get_teams(team_ids).await {
        Ok(teams) => teams,
        Err(err) if err.status_code == 404 => return Err(invalid()),
        Err(err) => return Err(ApiError::from(err)),
    };
    if team_ids
        .iter()
        .any(|id| !teams.iter().any(|team| &team.id == id))
    {
        return Err(invalid());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------------------------

/// Port of `createAccessControlPolicy` — `PUT /api/v4/access_control_policies`.
///
/// The body, the two feature flags, then a per-type permission rung — a team admin's parent
/// policy needs `?team_id` and, when the body carries an id, ownership; a channel policy needs
/// `manage_channel_access_rules` and the channel checks; a team policy the team permission and
/// the self-inclusion check per rule — and then the service, whose answer here is the 501.
#[tracing::instrument(skip_all, fields(policy_type, has_manage_system))]
pub async fn create_access_control_policy(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "policy").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let mut policy: AccessControlPolicy = match decode_struct(&bytes, "policy") {
        Ok(policy) => policy,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("policy_type", policy.type_.as_str());

    if policy.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
        && !state.app.config().feature_flag_permission_policies
    {
        return feature_disabled(
            "createAccessControlPolicy",
            "api.access_control_policy.permission_policies.feature_disabled",
        );
    }
    if policy.type_ == ACCESS_CONTROL_POLICY_TYPE_CHANNEL
        && policy.has_permission_rule_action()
        && !state.app.config().channel_permission_policies_enabled()
    {
        return feature_disabled(
            "createAccessControlPolicy",
            "api.access_control_policy.channel_permission_policies.feature_disabled",
        );
    }

    let has_manage_system = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    tracing::Span::current().record("has_manage_system", has_manage_system);

    match policy.type_.as_str() {
        ACCESS_CONTROL_POLICY_TYPE_PARENT => {
            let team_id = query_first(query.as_deref(), "team_id").unwrap_or_default();
            if !has_manage_system {
                if team_id.is_empty() || !is_valid_id(&team_id) {
                    return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
                }
                if !state
                    .app
                    .session_has_permission_to_team(
                        &session.0,
                        &team_id,
                        &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                    )
                    .await
                {
                    return permission_error(&session, &PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                if !policy.id.is_empty() {
                    match state
                        .app
                        .validate_team_admin_policy_ownership(&team_id, &policy.id)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            return permission_error(
                                &session,
                                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                            );
                        }
                        Err(err) => return ApiError::from(err).into_response(),
                    }
                }
            }
            // Scope stamping: a team admin's scope is always the query's team; a system admin's
            // only when the body left it empty.
            if !has_manage_system
                || (!team_id.is_empty() && is_valid_id(&team_id) && policy.scope.is_empty())
            {
                policy.scope = ACCESS_CONTROL_POLICY_SCOPE_TEAM.to_owned();
                policy.scope_id = team_id;
            }
        }
        ACCESS_CONTROL_POLICY_TYPE_PERMISSION => {
            if !has_manage_system {
                return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
            }
        }
        ACCESS_CONTROL_POLICY_TYPE_CHANNEL => {
            if !has_manage_system {
                if !is_valid_id(&policy.id) {
                    return ApiError::invalid_param("policy.id").into_response();
                }
                let (ok, _) = state
                    .app
                    .has_permission_to_channel(
                        &session.0.user_id,
                        &policy.id,
                        &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES,
                    )
                    .await;
                if !ok {
                    return permission_error(&session, &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES);
                }
                if let Err(err) = state
                    .app
                    .validate_channel_access_control_policy_creation(&session.0.user_id, &policy)
                    .await
                {
                    return ApiError::from(err).into_response();
                }
                if let Err(err) = preserve_system_managed_fields(&state, &mut policy) {
                    return err.into_response();
                }
            }
        }
        ACCESS_CONTROL_POLICY_TYPE_TEAM => {
            if !has_manage_system {
                if !is_valid_id(&policy.id) {
                    return ApiError::invalid_param("policy.id").into_response();
                }
                if !state
                    .app
                    .session_has_permission_to_team(
                        &session.0,
                        &policy.id,
                        &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                    )
                    .await
                {
                    return permission_error(&session, &PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                for rule in policy.rules.as_deref().unwrap_or_default() {
                    if let Err(err) = state
                        .app
                        .validate_team_admin_self_inclusion(&session.0.user_id, &rule.expression)
                    {
                        return ApiError::from(err).into_response();
                    }
                }
                if let Err(err) = preserve_system_managed_fields(&state, &mut policy) {
                    return err.into_response();
                }
            }
        }
        _ => return ApiError::invalid_param("type").into_response(),
    }

    match state.app.create_or_update_access_control_policy(&policy) {
        Ok(created) => marshalled("createAccessControlPolicy", &created),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `preserveSystemManagedFields` (api4/access_control.go:27): the stored policy's
/// imports and scope replace the body's; a **404** means a first-time create and empties them.
/// The read is the service's, so on this build this is the `get_policy` 501.
fn preserve_system_managed_fields(
    state: &AppState,
    policy: &mut AccessControlPolicy,
) -> Result<(), ApiError> {
    match state.app.get_access_control_policy(&policy.id) {
        Ok(stored) => {
            policy.imports = stored.imports.clone();
            policy.scope = stored.scope;
            policy.scope_id = stored.scope_id;
            Ok(())
        }
        Err(err) if err.status_code == 404 => {
            policy.imports = None;
            policy.scope = String::new();
            policy.scope_id = String::new();
            Ok(())
        }
        Err(err) => Err(ApiError::from(err)),
    }
}

/// Port of `getAccessControlPolicy` — `GET /api/v4/access_control_policies/{policy_id}`.
///
/// A system admin goes straight to the service. Anyone else is tried as a channel admin first
/// (`ValidateAccessControlPolicyPermissionWithChannelContext`, read-only, with `?channelId`) —
/// which on this build is the 501 and so a refusal — and then as a team admin through
/// `?team_id` and ownership.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn get_access_control_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }
    let query = query.as_deref();
    let channel_id = query_first(query, "channelId").unwrap_or_default();

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
        && state
            .app
            .validate_access_control_policy_permission_with_options(
                &session.0.user_id,
                &policy_id,
                true,
                &channel_id,
            )
            .await
            .is_err()
        && let Err(response) =
            team_admin_owning(&state, &session, query_first(query, "team_id"), &policy_id).await
    {
        return response.into_response();
    }

    // `PopulateAccessControlPolicyChildCounts` and the masking would follow a successful read.
    match state.app.get_access_control_policy(&policy_id) {
        Ok(policy) => marshalled("getAccessControlPolicy", &policy),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `deleteAccessControlPolicy` — `DELETE /api/v4/access_control_policies/{policy_id}`.
///
/// The team-admin rung differs from the read's: `?team_id` is required outright (its absence is
/// `manage_system`), and a policy whose id **is** the team's skips the ownership check — a team
/// admin may delete their own team's policy.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn delete_access_control_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
        && state
            .app
            .validate_access_control_policy_permission(&session.0.user_id, &policy_id)
            .await
            .is_err()
    {
        let team_id = query_first(query.as_deref(), "team_id").unwrap_or_default();
        if team_id.is_empty() || !is_valid_id(&team_id) {
            return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
        }
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await
        {
            return permission_error(&session, &PERMISSION_MANAGE_TEAM_ACCESS_RULES);
        }
        if policy_id != team_id {
            match state
                .app
                .validate_team_admin_policy_ownership(&team_id, &policy_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return permission_error(&session, &PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                Err(err) => return ApiError::from(err).into_response(),
            }
        }
    }

    match state.app.delete_access_control_policy(&policy_id) {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `checkExpression` — `POST /api/v4/access_control_policies/cel/check`. The body's
/// parameter name on a decode failure is `user`, as in Go.
#[tracing::instrument(skip_all)]
pub async fn check_expression(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "user").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let body: ExpressionRequest = match decode_struct(&bytes, "user") {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    if !body.channel_id.is_empty() && !is_valid_id(&body.channel_id) {
        return ApiError::invalid_param("channelId").into_response();
    }
    if let Err(response) = cel_tooling_gate(&state, &session, &body.channel_id, &body.team_id).await
    {
        return response.into_response();
    }
    match state.app.check_expression(&body.expression) {
        Ok(()) => marshalled("checkExpression", &Vec::<()>::new()),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `testExpression` — `POST /api/v4/access_control_policies/cel/test`.
///
/// Both ids are validated before the gate, and the three lanes — system admin, delegated team
/// admin, channel admin — reach three app functions that on this build are one 501.
#[tracing::instrument(skip_all, fields(lane))]
pub async fn test_expression(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "user").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let body: QueryExpressionParams = match decode_struct(&bytes, "user") {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    if !body.channel_id.is_empty() && !is_valid_id(&body.channel_id) {
        return ApiError::invalid_param("channelId").into_response();
    }
    if !body.team_id.is_empty() && !is_valid_id(&body.team_id) {
        return ApiError::invalid_param("teamId").into_response();
    }

    let has_system_permission = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    let has_team_permission = !has_system_permission
        && team_admin_cel_context_ok(&state, &session, &body.channel_id, &body.team_id).await;
    if !has_system_permission && !has_team_permission {
        if body.channel_id.is_empty() {
            return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
        }
        let (ok, _) = state
            .app
            .has_permission_to_channel(
                &session.0.user_id,
                &body.channel_id,
                &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES,
            )
            .await;
        if !ok {
            return permission_error(&session, &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES);
        }
    }

    let result = if has_system_permission {
        tracing::Span::current().record("lane", "system");
        state.app.test_expression(&body.expression)
    } else {
        tracing::Span::current().record(
            "lane",
            if has_team_permission {
                "team"
            } else {
                "channel"
            },
        );
        // `TestExpressionWith{Team,Channel}Context`: the requester check comes first.
        state
            .app
            .validate_expression_against_requester(&body.expression, &session.0.user_id)
            .map(|_| ())
    };
    match result {
        Ok(()) => marshalled(
            "checkExpression",
            &mm_model::access_policy::AccessControlPolicyTestResponse::default(),
        ),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `simulatePolicyForUsers` — `POST /api/v4/access_control_policies/cel/simulate_users`.
///
/// The feature flag, the body and its five validations, the CEL gate, the cross-team check
/// (which resolves the channel for everyone, a 404 when it is missing), the delegated caller's
/// users-in-scope check, then the service.
#[tracing::instrument(skip_all)]
pub async fn simulate_policy_for_users(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().policy_simulation_enabled() {
        return feature_disabled(
            "simulatePolicyForUsers",
            "api.access_control_policy.policy_simulation.feature_disabled",
        );
    }
    let bytes = match body_bytes(request, "simulation").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let mut params: PolicySimulationByUsersParams = match decode_struct(&bytes, "simulation") {
        Ok(params) => params,
        Err(err) => return err.into_response(),
    };
    if params.policy.is_none() {
        return ApiError::invalid_param("policy").into_response();
    }
    let users = params.users.take().unwrap_or_default();
    if users.is_empty() {
        return ApiError::invalid_param("users").into_response();
    }
    if !params.channel_id.is_empty() && !is_valid_id(&params.channel_id) {
        return ApiError::invalid_param("channel_id").into_response();
    }
    if !params.team_id.is_empty() && !is_valid_id(&params.team_id) {
        return ApiError::invalid_param("team_id").into_response();
    }
    match params.evaluation_scope.as_str() {
        "" | POLICY_EVALUATION_SCOPE_THIS_RULE | POLICY_EVALUATION_SCOPE_ALL => {}
        _ => return ApiError::invalid_param("evaluation_scope").into_response(),
    }
    if params.evaluation_scope.is_empty() {
        params.evaluation_scope = POLICY_EVALUATION_SCOPE_THIS_RULE.to_owned();
    }

    let has_system_permission =
        match cel_tooling_gate(&state, &session, &params.channel_id, &params.team_id).await {
            Ok(has_system_permission) => has_system_permission,
            Err(response) => return response.into_response(),
        };

    if !params.channel_id.is_empty() && !params.team_id.is_empty() {
        let channel = match state.app.get_channel(&params.channel_id).await {
            Ok(channel) => channel,
            Err(err) => return ApiError::from(err).into_response(),
        };
        if channel.team_id != params.team_id {
            return ApiError::invalid_param("team_id").into_response();
        }
        params.team_id = channel.team_id;
    }

    if !has_system_permission
        && let Err(err) = state
            .app
            .validate_policy_simulation_users_in_scope(&params.team_id, &params.channel_id, &users)
            .await
    {
        return ApiError::from(err).into_response();
    }

    match state.app.simulate_access_control_policy_for_users() {
        Ok(()) => marshalled("simulatePolicyForUsers", &serde_json::Value::Null),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `validateExpressionAgainstRequester` —
/// `POST /api/v4/access_control_policies/cel/validate_requester`. Both ids validated, the CEL
/// gate, then the service; a success would be `json.NewEncoder`-written.
#[tracing::instrument(skip_all)]
pub async fn validate_expression_against_requester(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "request").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let body: ExpressionRequest = match decode_struct(&bytes, "request") {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    if !body.channel_id.is_empty() && !is_valid_id(&body.channel_id) {
        return ApiError::invalid_param("channelId").into_response();
    }
    if !body.team_id.is_empty() && !is_valid_id(&body.team_id) {
        return ApiError::invalid_param("teamId").into_response();
    }
    if let Err(response) = cel_tooling_gate(&state, &session, &body.channel_id, &body.team_id).await
    {
        return response.into_response();
    }
    match state
        .app
        .validate_expression_against_requester(&body.expression, &session.0.user_id)
    {
        Ok(matches) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            format!("{{\"requester_matches\":{matches}}}\n"),
        )
            .into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `getFieldsAutocomplete` —
/// `GET /api/v4/access_control_policies/cel/autocomplete/fields`.
///
/// `channelId` validated, the CEL gate (`team_id` is the query's), then `after` (empty becomes
/// the 26-zero sentinel) and `limit`, whose absence, unparseability, `<= 0` and `> 100` are one
/// 400. The answer is the property fields the caller may see, with the four native descriptors
/// on the first page — served in full, since nothing here needs the service.
#[tracing::instrument(skip_all, fields(limit, found))]
pub async fn get_fields_autocomplete(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    let query = query.as_deref();
    let channel_id = query_first(query, "channelId").unwrap_or_default();
    if !channel_id.is_empty() && !is_valid_id(&channel_id) {
        return ApiError::invalid_param("channelId").into_response();
    }
    let team_id = query_first(query, "team_id").unwrap_or_default();
    if let Err(response) = cel_tooling_gate(&state, &session, &channel_id, &team_id).await {
        return response.into_response();
    }

    let mut after = query_first(query, "after").unwrap_or_default();
    if !after.is_empty() && !is_valid_id(&after) {
        return ApiError::invalid_param("after").into_response();
    } else if after.is_empty() {
        after = "0".repeat(26);
    }

    let limit_refusal = || {
        ApiError::from(AppError::new(
            "getFieldsAutocomplete",
            "api.access_control_policy.get_fields.limit.app_error",
            None,
            String::new(),
            400,
        ))
    };
    let limit = match atoi(&query_first(query, "limit").unwrap_or_default()) {
        Some(limit) if limit > 0 && limit <= 100 => limit,
        _ => return limit_refusal().into_response(),
    };
    tracing::Span::current().record("limit", limit);

    match state
        .app
        .get_access_control_fields_autocomplete(&after, limit, &session.0.user_id)
        .await
    {
        Ok(fields) => {
            tracing::Span::current().record("found", fields.len());
            marshalled("getExpressionAutocomplete", &fields)
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `convertToVisualAST` — `POST /api/v4/access_control_policies/cel/visual_ast`. Only
/// `channelId` is validated here (not `teamId`), then the CEL gate, then the service — masked or
/// not, `ExpressionToVisualAST` runs first.
#[tracing::instrument(skip_all)]
pub async fn convert_to_visual_ast(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "user").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let body: ExpressionRequest = match decode_struct(&bytes, "user") {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    if !body.channel_id.is_empty() && !is_valid_id(&body.channel_id) {
        return ApiError::invalid_param("channelId").into_response();
    }
    if let Err(response) = cel_tooling_gate(&state, &session, &body.channel_id, &body.team_id).await
    {
        return response.into_response();
    }
    match state.app.expression_to_visual_ast(&body.expression) {
        Ok(()) => marshalled("convertToVisualAST", &serde_json::Value::Null),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `searchAccessControlPolicies` — `POST /api/v4/access_control_policies/search`.
///
/// A `null` or undecodable body is the 400. With a `team_id`: valid, and the session manages
/// access rules in it — a system admin implicitly does — then the team search. Without: a
/// `permission` type needs the flag, the caller needs `manage_system`, then the system search.
/// Both searches are the service's, so both are the 501 here.
#[tracing::instrument(skip_all, fields(team_id, policy_type))]
pub async fn search_access_control_policies(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "access_control_policy_search").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let props: AccessControlPolicySearch =
        match decode_pointer(&bytes, "access_control_policy_search") {
            Ok(props) => props,
            Err(err) => return err.into_response(),
        };
    tracing::Span::current().record("team_id", props.team_id.as_str());
    tracing::Span::current().record("policy_type", props.type_.as_str());

    let result = if !props.team_id.is_empty() {
        if !is_valid_id(&props.team_id) {
            return ApiError::invalid_param("team_id").into_response();
        }
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &props.team_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await
        {
            return permission_error(&session, &PERMISSION_MANAGE_TEAM_ACCESS_RULES);
        }
        // `SearchTeamAccessPolicies`: `SearchAccessControlPolicies` is its first call.
        state.app.search_access_control_policies()
    } else {
        if props.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
            && !state.app.config().feature_flag_permission_policies
        {
            return feature_disabled(
                "searchAccessControlPolicies",
                "api.access_control_policy.permission_policies.feature_disabled",
            );
        }
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
        {
            return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
        }
        state.app.search_access_control_policies()
    };
    match result {
        Ok(()) => marshalled(
            "searchAccessControlPolicies",
            &mm_model::access_policy::AccessControlPoliciesWithCount::default(),
        ),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateActiveStatus` — `GET /api/v4/access_control_policies/{policy_id}/activate`,
/// the deprecated single-policy toggle.
///
/// After the id: the CSRF barrier — a session presented as a **cookie** is the 401
/// `api.context.session_cookie_not_allowed.app_error` — then the permission (a non-admin's
/// delegated check fails on this build and is refused as `manage_system`), then `active`,
/// which must be the literal `true` or `false`, then the service.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn update_active_status(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }

    let (parts, _) = request.into_parts();
    match parse_auth_token(&parts) {
        Some((token, location)) if !token.is_empty() && location != TokenLocation::Cookie => {}
        _ => {
            return ApiError::from(AppError::new(
                "updateActiveStatus",
                "api.context.session_cookie_not_allowed.app_error",
                None,
                "This endpoint requires header-based authentication",
                401,
            ))
            .into_response();
        }
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
        && state
            .app
            .validate_access_control_policy_permission(&session.0.user_id, &policy_id)
            .await
            .is_err()
    {
        return permission_error(&session, &PERMISSION_MANAGE_SYSTEM);
    }

    let active = query_first(query.as_deref(), "active").unwrap_or_default();
    if active != "true" && active != "false" {
        return ApiError::invalid_param("active").into_response();
    }

    match state.app.update_access_control_policies_active() {
        Ok(()) => {
            let mut response = (
                StatusCode::OK,
                [
                    ("Content-Type", "application/json"),
                    ("x-mmrs-served-by", "rust"),
                ],
                "{\"status\":\"OK\"}\n",
            )
                .into_response();
            response
                .headers_mut()
                .insert("Deprecation", HeaderValue::from_static("true"));
            response.headers_mut().insert(
                header::LINK,
                HeaderValue::from_static(
                    "</api/v4/access_control/policies/activate>; rel=\"successor-version\"",
                ),
            );
            response
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `setActiveStatus` — `PUT /api/v4/access_control_policies/activate`, the batch
/// toggle.
///
/// A non-admin is checked **per entry**: the delegated channel check (a refusal here), then
/// the body's `team_id` with ownership — skipped when the entry *is* the team — and otherwise
/// `manage_channel_access_rules`. No entries, no checks: the 501 for anyone.
#[tracing::instrument(skip_all, fields(entries))]
pub async fn set_active_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match body_bytes(request, "request").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let list: AccessControlPolicyActiveUpdateRequest = match decode_struct(&bytes, "request") {
        Ok(list) => list,
        Err(err) => return err.into_response(),
    };
    let entries = list.entries.unwrap_or_default();
    tracing::Span::current().record("entries", entries.len());

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        for entry in &entries {
            if state
                .app
                .validate_access_control_policy_permission(&session.0.user_id, &entry.id)
                .await
                .is_ok()
            {
                continue;
            }
            if !list.team_id.is_empty()
                && is_valid_id(&list.team_id)
                && state
                    .app
                    .session_has_permission_to_team(
                        &session.0,
                        &list.team_id,
                        &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                    )
                    .await
            {
                if entry.id != list.team_id {
                    match state
                        .app
                        .validate_team_admin_policy_ownership(&list.team_id, &entry.id)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            return permission_error(
                                &session,
                                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                            );
                        }
                        Err(err) => return ApiError::from(err).into_response(),
                    }
                }
            } else {
                return permission_error(&session, &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES);
            }
        }
    }

    match state.app.update_access_control_policies_active() {
        Ok(()) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            "[]\n",
        )
            .into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The permission rung `assignAccessPolicy` and `unassignAccessPolicy` share.
///
/// `team_ids` first: the feature gate (`TeamMembershipAccessControlEnabled`, false on this
/// stack licensed or not) is a 501 before anything, and past it only a system admin may name
/// teams. Then a non-admin needs a valid `team_id`, the team permission, ownership, and every
/// `channel_ids` entry validated against the team.
async fn assignment_gate(
    state: &AppState,
    session: &AuthenticatedSession,
    policy_id: &str,
    assignments: &Assignments,
    where_: &'static str,
) -> Result<(), ApiError> {
    let team_ids = assignments.team_ids.as_deref().unwrap_or_default();
    if !team_ids.is_empty() {
        match state.app.team_membership_access_control_enabled().await {
            Ok(true) => {}
            Ok(false) => {
                return Err(feature_disabled_refusal(
                    where_,
                    "api.access_control_policy.team_membership.feature_disabled",
                ));
            }
            Err(err) => return Err(ApiError::from(*err)),
        }
    }

    let has_system_permission = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    if !team_ids.is_empty() && !has_system_permission {
        return Err(permission_refusal(session, &PERMISSION_MANAGE_SYSTEM));
    }
    if has_system_permission {
        return Ok(());
    }

    if assignments.team_id.is_empty() || !is_valid_id(&assignments.team_id) {
        return Err(permission_refusal(session, &PERMISSION_MANAGE_SYSTEM));
    }
    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &assignments.team_id,
            &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
        )
        .await
    {
        return Err(permission_refusal(
            session,
            &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
        ));
    }
    match state
        .app
        .validate_team_admin_policy_ownership(&assignments.team_id, policy_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(permission_refusal(
                session,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            ));
        }
        Err(err) => return Err(ApiError::from(err)),
    }
    state
        .app
        .validate_team_scope_policy_channel_assignment(
            &assignments.team_id,
            assignments.channel_ids.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(ApiError::from)
}

/// Port of `assignAccessPolicy` — `POST /api/v4/access_control_policies/{policy_id}/assign`.
///
/// After the gate: `team_ids` must exist, then the channel assignment (the 501 when any channel
/// is named), then the team assignment (its 501), then the scope reconcile — whose failure is
/// only logged — and `{"status":"OK"}`. So a body naming nothing is a 200 on this build, and the
/// reconcile it runs is real.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn assign_access_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }
    let bytes = match body_bytes(request, "assignments").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let assignments: Assignments = match decode_struct(&bytes, "assignments") {
        Ok(assignments) => assignments,
        Err(err) => return err.into_response(),
    };
    if let Err(response) = assignment_gate(
        &state,
        &session,
        &policy_id,
        &assignments,
        "assignAccessPolicy",
    )
    .await
    {
        return response.into_response();
    }

    let team_ids = assignments.team_ids.as_deref().unwrap_or_default();
    if let Err(err) = validate_team_ids_exist(&state, team_ids).await {
        return err.into_response();
    }
    if !assignments
        .channel_ids
        .as_deref()
        .unwrap_or_default()
        .is_empty()
        && let Err(err) = state.app.assign_access_control_policy_to_channels()
    {
        return ApiError::from(err).into_response();
    }
    if !team_ids.is_empty()
        && let Err(err) = state.app.assign_access_control_policy_to_teams()
    {
        return ApiError::from(err).into_response();
    }
    if let Err(err) = state.app.reconcile_policy_team_scope(&policy_id).await {
        tracing::warn!(policy_id = %policy_id, error = %err.id, "Failed to reconcile policy team scope after assign");
    }
    status_ok()
}

/// Port of `unassignAccessPolicy` — `DELETE /api/v4/access_control_policies/{policy_id}/unassign`.
///
/// The same gate; then `team_ids` must exist, a **pre-flight** reconcile (logged on failure),
/// the channel unassignment (its 501), the team unassignment (its 501), the post reconcile and
/// `{"status":"OK"}`.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn unassign_access_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }
    let bytes = match body_bytes(request, "assignments").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let assignments: Assignments = match decode_struct(&bytes, "assignments") {
        Ok(assignments) => assignments,
        Err(err) => return err.into_response(),
    };
    if let Err(response) = assignment_gate(
        &state,
        &session,
        &policy_id,
        &assignments,
        "unassignAccessPolicy",
    )
    .await
    {
        return response.into_response();
    }

    let team_ids = assignments.team_ids.as_deref().unwrap_or_default();
    if let Err(err) = validate_team_ids_exist(&state, team_ids).await {
        return err.into_response();
    }
    if let Err(err) = state.app.reconcile_policy_team_scope(&policy_id).await {
        tracing::warn!(policy_id = %policy_id, error = %err.id, "Failed to reconcile policy team scope before unassign");
    }
    if !assignments
        .channel_ids
        .as_deref()
        .unwrap_or_default()
        .is_empty()
        && let Err(err) = state.app.unassign_policies_from_channels()
    {
        return ApiError::from(err).into_response();
    }
    if !team_ids.is_empty()
        && let Err(err) = state.app.unassign_policies_from_teams()
    {
        return ApiError::from(err).into_response();
    }
    if let Err(err) = state.app.reconcile_policy_team_scope(&policy_id).await {
        tracing::warn!(policy_id = %policy_id, error = %err.id, "Failed to reconcile policy team scope after unassign");
    }
    status_ok()
}

/// Port of `getChannelsForAccessControlPolicy` —
/// `GET /api/v4/access_control_policies/{policy_id}/resources/channels`.
///
/// The read's team-admin rung, then `after` (a valid id or empty), then `limit` — required
/// and `strconv.Atoi`-parsed, its own 400 — then the service.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id))]
pub async fn get_channels_for_access_control_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }
    let query = query.as_deref();
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
        && let Err(response) =
            team_admin_owning(&state, &session, query_first(query, "team_id"), &policy_id).await
    {
        return response.into_response();
    }

    let after = query_first(query, "after").unwrap_or_default();
    if !after.is_empty() && !is_valid_id(&after) {
        return ApiError::invalid_param("after").into_response();
    }
    if atoi(&query_first(query, "limit").unwrap_or_default()).is_none() {
        return ApiError::from(AppError::new(
            "getChannelsForAccessControlPolicy",
            "api.access_control_policy.get_channels.limit.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match state.app.get_channels_for_policy(&policy_id) {
        Ok(()) => marshalled(
            "getChannelsForAccessControlPolicy",
            &ChannelsWithCount::default(),
        ),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `searchChannelsForAccessControlPolicy` —
/// `POST /api/v4/access_control_policies/{policy_id}/resources/channels/search`.
///
/// The permission rung comes **before** the body is decoded, so a member with an unreadable
/// body is a 403. Then `SearchAllChannels` with `ExcludeGroupConstrained` forced on, the team
/// list forced to the authorised team for a delegated caller, and the policy id as the parent
/// filter — served in full, with `total_count` always 0 because the search is unpaginated.
#[tracing::instrument(skip_all, fields(policy_id = %policy_id, found))]
pub async fn search_channels_for_access_control_policy(
    State(state): State<AppState>,
    Path(policy_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&policy_id, "policy_id") {
        return err.into_response();
    }
    let has_system_permission = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;
    let authorized_team_id = if has_system_permission {
        String::new()
    } else {
        match team_admin_owning(
            &state,
            &session,
            query_first(query.as_deref(), "team_id"),
            &policy_id,
        )
        .await
        {
            Ok(team_id) => team_id,
            Err(response) => return response.into_response(),
        }
    };

    let props = match decode_full_channel_search(request).await {
        Ok(props) => props,
        Err(err) => return err.into_response(),
    };

    let team_ids = if !has_system_permission && !authorized_team_id.is_empty() {
        vec![authorized_team_id]
    } else {
        props.team_ids.clone().unwrap_or_default()
    };
    let opts = ChannelSearchOpts {
        deleted: props.deleted,
        include_deleted: props.include_deleted,
        exclude_group_constrained: true,
        team_ids,
        parent_access_control_policy_id: policy_id,
        ..ChannelSearchOpts::default()
    };
    match state.app.search_all_channels(&props.term, &opts).await {
        Ok((channels, total_count)) => {
            tracing::Span::current().record("found", channels.0.len());
            marshalled(
                "searchChannelsInPolicy",
                &ChannelsWithCount {
                    channels: Some(channels),
                    total_count,
                },
            )
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `strconv.Atoi`: signs yes, whitespace and empty no.
    #[test]
    fn atoi_is_strconv_atoi() {
        assert_eq!(atoi("10"), Some(10));
        assert_eq!(atoi("+7"), Some(7));
        assert_eq!(atoi("-3"), Some(-3));
        assert_eq!(atoi("007"), Some(7));
        assert_eq!(atoi(""), None);
        assert_eq!(atoi(" 5"), None);
        assert_eq!(atoi("5x"), None);
        assert_eq!(atoi("1_0"), None);
        assert_eq!(atoi("99999999999999999999"), None);
    }

    /// A struct decode: a `null` body is the zero value; a pointer decode: it is the 400.
    #[test]
    fn null_bodies_decode_like_go() {
        let request: ExpressionRequest = decode_struct(b"null", "user").unwrap();
        assert_eq!(request.expression, "");
        assert!(decode_pointer::<AccessControlPolicySearch>(b"null", "s").is_err());
        assert!(decode_pointer::<AccessControlPolicySearch>(b"{}", "s").is_ok());
        assert!(decode_struct::<ExpressionRequest>(b"{\"expression\":", "user").is_err());
        // One value, trailing bytes ignored.
        let request: ExpressionRequest =
            decode_struct(b"{\"expression\":\"true\"} trailing", "user").unwrap();
        assert_eq!(request.expression, "true");
    }
}
