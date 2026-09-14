//! The three access-control reads a channel or team member can make —
//! `getChannelAccessControlAttributes` (api4/channel.go:3266), `getTeamAccessControlAttributes`
//! (api4/team.go:276) and `getTeamAccessControlPolicy` (api4/team.go:181) — on a build whose
//! access-control service is **nil**.
//!
//! `ch.AccessControl` is assigned only when the enterprise package registered
//! `AccessControlServiceInterface` (app/channels.go:203), which nothing in this tree does
//! ([D-571]). So `GetAccessControlPolicyAttributes` and `GetAccessControlPolicy` both answer
//! the 501 "Policy Administration Point is not initialized" before reading anything, and the
//! routes reduce to their permission check plus that answer — or, for the policy read, the empty
//! document `TeamMembershipAccessControlEnabled() == false` selects before the service is asked.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM_ACCESS_RULES, PERMISSION_READ_CHANNEL,
    PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::proxy;

/// `GetAccessControlPolicyAttributes`'s answer with no service (app/access_control.go:1689):
/// the `where` is `GetChannelAccessControlAttributes` for the team route too, since both
/// handlers call the same function.
fn policy_administration_point_not_initialized() -> ApiError {
    const NOT_INITIALIZED_DETAIL: &str = "Policy Administration Point is not initialized";
    ApiError::from(AppError::new(
        "GetChannelAccessControlAttributes",
        "app.pap.get_channel_access_control_attributes.app_error",
        None,
        NOT_INITIALIZED_DETAIL,
        501,
    ))
}

/// Port of `getChannelAccessControlAttributes` — `GET /api/v4/channels/{channel_id}/access_control/attributes`.
///
/// `read_channel` on the channel; then, with `EnableChannelPolicyIndicators` off, an
/// encoder-written `{}` so no attribute value leaks to members; otherwise the service, which is
/// the 501 here.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
pub async fn get_channel_access_control_attributes(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    let (has_permission, _) = state
        .app
        .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    if !has_permission {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL],
        ))
        .into_response();
    }

    if !state.app.config().enable_channel_policy_indicators {
        return (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            "{}\n",
        )
            .into_response();
    }

    policy_administration_point_not_initialized().into_response()
}

/// Port of `getTeamAccessControlAttributes` — `GET /api/v4/teams/{team_id}/access_control/attributes`.
///
/// `view_team`, then the service — the 501.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn get_team_access_control_attributes(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }
    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(&session.0, &[&PERMISSION_VIEW_TEAM]))
            .into_response();
    }

    policy_administration_point_not_initialized().into_response()
}

/// Port of `getTeamAccessControlPolicy` — `GET /api/v4/teams/{team_id}/access_control/policy`.
///
/// `manage_system` **or** `manage_team_access_rules` on the team, the refusal naming the
/// latter. Then `TeamMembershipAccessControlEnabled` — an Enterprise Advanced licence and the
/// ABAC setting together — decides: off, the `json.Marshal`-written
/// `{"policy":null,"enforced":false}` with no trailing newline; on, the enforcement read and
/// the policy, which is the service's and is forwarded.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn get_team_access_control_policy(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const OFF: &[u8] = br#"{"policy":null,"enforced":false}"#;

    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
        && !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
            )
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM_ACCESS_RULES],
        ))
        .into_response();
    }

    match state.app.team_membership_access_control_enabled().await {
        Ok(false) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            OFF,
        )
            .into_response(),
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!("handing an attribute-based team policy read to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(*err).into_response(),
    }
}
