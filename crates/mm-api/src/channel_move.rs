//! Port of `moveChannel` (api4/channel.go) — `POST /api/v4/channels/{channel_id}/move`, the
//! system-admin route that re-homes a channel on another team.
//!
//! # The order is the wire format
//!
//! The channel is loaded **before** the body is read, so an unknown channel is the 404 even with
//! an empty body; the body's two faults come next, each its own 400 (`team_id` must be a JSON
//! string, `force` a JSON bool — a `"true"` is a `force` fault); the team's 404 follows; then the
//! DM/GM refusal at **403** `api.channel.move_channel.type.invalid`; and only then the permission
//! — so a plain member learns whether the channel and team exist before being refused. Every
//! caller who passes those is the admin, so the two sweeps and the move run as them.
//!
//! # What is forwarded
//!
//! A member whose removal this port cannot reproduce (a guest, a shared channel — see
//! [`mm_app::App::remove_user_from_channel`]) makes the sweep a [`MemberWrite::Forward`], and the
//! request is handed to Go whole. The deactivated-member sweep may already have run by then; it
//! is a plain `DELETE` Go repeats without effect.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_member::MemberWrite;
use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::proxy;

/// `model.StringInterfaceFromJSON(r.Body)`: an object, or an empty map for anything else.
fn string_interface_from_json(bytes: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// Port of `moveChannel` — `POST /api/v4/channels/{channel_id}/move`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, forwarded = false))]
pub async fn move_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError::from(*err).into_response(),
    };

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let props = string_interface_from_json(&bytes);
    let Some(team_id) = props.get("team_id").and_then(|v| v.as_str()) else {
        return ApiError::invalid_param("team_id").into_response();
    };
    let Some(force) = props.get("force").and_then(|v| v.as_bool()) else {
        return ApiError::invalid_param("force").into_response();
    };

    let team = match state.app.get_team(team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(*err).into_response(),
    };

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return ApiError::from(AppError::new(
            "moveChannel",
            "api.channel.move_channel.type.invalid",
            None,
            String::new(),
            403,
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let user = match state.app.get_user(&session.0.user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(*err).into_response(),
    };

    if let Err(err) = state
        .app
        .remove_all_deactivated_members_from_channel(&channel)
        .await
    {
        return ApiError::from(*err).into_response();
    }

    let forward = |state: AppState, why: &'static str| async move {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!(reason = why, "handing a channel move to Go");
        let request = Request::from_parts(parts, axum::body::Body::from(bytes.clone()));
        proxy::forward_to_go(State(state), request).await
    };

    if force {
        match state
            .app
            .remove_users_from_channel_not_member_of_team(&user, &channel, &team)
            .await
        {
            Ok(MemberWrite::Done(())) => {}
            Ok(MemberWrite::Forward(why)) => return forward(state, why).await,
            Err(err) => return ApiError::from(*err).into_response(),
        }
    }

    match state.app.move_channel(&team, &mut channel, &user).await {
        Ok(MemberWrite::Done(())) => {}
        Ok(MemberWrite::Forward(why)) => return forward(state, why).await,
        Err(err) => return ApiError::from(*err).into_response(),
    }

    let mut body = match serde_json::to_vec(&channel) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the moved channel");
            return ApiError::from(AppError::new(
                "moveChannel",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    // `json.NewEncoder(w).Encode(channel)` — with the trailing newline.
    body.push(b'\n');
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
