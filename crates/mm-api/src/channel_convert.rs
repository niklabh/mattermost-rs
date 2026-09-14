//! Port of `convertGroupMessageToChannel` (api4/channel.go) —
//! `POST /api/v4/channels/{channel_id}/convert_to_channel`.
//!
//! # The order
//!
//! The id; the board and space rejections; the body — `json.NewDecoder(r.Body).Decode(&ptr)`,
//! so a `null` body leaves the pointer nil and is the same 400 `body` as a decode error; the
//! caller, whose **guest** flag is a 403 `not_allowed_for_user.request_error` before any
//! permission is asked; `create_private_channel` on the *body's* team; and the body's
//! `channel_id` matching the path's, a 400 `channel_id`. Only then the app, whose own order is
//! the module doc of [`mm_app::channel_convert`].

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_member::MemberWrite;
use mm_model::channel::GroupMessageConversionRequestBody;
use mm_model::permission::{PERMISSION_CREATE_PRIVATE_CHANNEL, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channel_member_writes::reject_board_or_space_channel;
use crate::channels::require_id;
use crate::error::ApiError;
use crate::proxy;

/// Port of `convertGroupMessageToChannel` — `POST /api/v4/channels/{channel_id}/convert_to_channel`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, forwarded = false))]
pub async fn convert_group_message_to_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }
    if let Some(err) = reject_board_or_space_channel(&state, &channel_id).await {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    // `json.NewDecoder(r.Body).Decode(&ptr)`: only an object decodes into the struct — a
    // `null` leaves the pointer nil, and a list or a scalar is a decode error — and serde would
    // otherwise accept `[]` as an all-default struct.
    let conversion = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value @ serde_json::Value::Object(_)) => {
            match serde_json::from_value::<GroupMessageConversionRequestBody>(value) {
                Ok(conversion) => conversion,
                Err(_) => return ApiError::invalid_param("body").into_response(),
            }
        }
        _ => return ApiError::invalid_param("body").into_response(),
    };

    let user = match state.app.get_user(&session.0.user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(*err).into_response(),
    };
    if user.is_guest() {
        return ApiError::from(AppError::new(
            "Api4.convertGroupMessageToChannel",
            "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
            None,
            format!("userId={}", session.0.user_id),
            403,
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &conversion.team_id,
            &PERMISSION_CREATE_PRIVATE_CHANNEL,
        )
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_PRIVATE_CHANNEL],
        ))
        .into_response();
    }

    // The channel id the payload must be the same one as indicated in the URL.
    if conversion.channel_id != channel_id {
        return ApiError::invalid_param("channel_id").into_response();
    }

    let updated = match state
        .app
        .convert_group_message_to_channel(&session.0.user_id, &conversion)
        .await
    {
        Ok(MemberWrite::Done(updated)) => updated,
        Ok(MemberWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing a group-message conversion to Go");
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(err) => return ApiError::from(*err).into_response(),
    };

    let mut body = match serde_json::to_vec(&updated) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the converted channel");
            return ApiError::from(AppError::new(
                "convertGroupMessageToChannel",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    // `json.NewEncoder(w).Encode(updatedChannel)` — with the trailing newline.
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
