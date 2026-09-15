//! Port of `api4/board.go` — the whole file, one route:
//!
//! ```text
//! POST /api/v4/boards   createBoard
//! ```
//!
//! # Registered only when `IntegratedBoards` is on, like `view.go`
//!
//! `InitBoard` (board.go:14) is one `if` on the feature flag, and the flag is **false** at the
//! pinned SHA, so on the stack's Go the mux has never heard of `/boards` and answers its own 404
//! `api.context.404.app_error`. The port follows [`crate::views`]: the flag is the first
//! statement of the handler, and when it is off the request is **forwarded**, so Go writes its
//! own 404. When it is on — `scripts/go-boards.sh` runs the oracle — the whole route is served.
//!
//! # The handler's order
//!
//! 1. decode the body into a `Channel` **value** — so `null` is a zero channel, not an error —
//!    and a body that does not decode is the 400 `invalid_body_param` naming `body`;
//! 2. `IsBoard()`, else the same 400 naming `type`; then an empty `team_id`, naming `team_id`;
//! 3. `create_public_channel` on the team for an open board (`BO`), `create_private_channel`
//!    for a private one (`BP`) — a plain user outside the team is the 403 either way;
//! 4. `creator_id` is the caller's, whatever the body said;
//! 5. [`mm_app::App::create_board_channel`], and a **201** with the saved channel through
//!    `json.NewEncoder`, so with a trailing newline.
//!
//! `IsValidBoard` (the trimmed `display_name`) and `Channel.IsValid` (a `name` of two or more
//! characters, among the rest) are the app's and the store's; a body without a `name` is the
//! store's `model.channel.is_valid.2_or_more.app_error` on both servers.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::channel::Channel;
use mm_model::permission::{
    PERMISSION_CREATE_PRIVATE_CHANNEL, PERMISSION_CREATE_PUBLIC_CHANNEL, make_permission_error,
};
use mm_model::utils::decode_one_from_json;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `createBoard` (board.go:20) — `POST /api/v4/boards`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, team_id, channel_type, forwarded = false))]
pub async fn create_board(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().feature_flag_integrated_boards {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the board body");
            return ApiError::invalid_param("body").into_response();
        }
    };
    match serve_create_board(&state, &session, &bytes).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_create_board(
    state: &AppState,
    session: &AuthenticatedSession,
    bytes: &[u8],
) -> Result<Response, ApiError> {
    // `var channel model.Channel; json.NewDecoder(r.Body).Decode(&channel)` — a value, so a
    // `null` body leaves the zero channel and is refused on its type, not its syntax.
    let mut channel = match decode_one_from_json::<Option<Channel>>(bytes) {
        Ok(channel) => channel.unwrap_or_default(),
        Err(err) => {
            tracing::debug!(error = %err, "board body did not decode");
            return Err(ApiError::invalid_param("body"));
        }
    };
    tracing::Span::current().record("channel_type", channel.channel_type.as_str());
    tracing::Span::current().record("team_id", channel.team_id.as_str());

    if !channel.is_board() {
        return Err(ApiError::invalid_param("type"));
    }
    if channel.team_id.is_empty() {
        return Err(ApiError::invalid_param("team_id"));
    }

    let permission = if channel.is_open_board() {
        &PERMISSION_CREATE_PUBLIC_CHANNEL
    } else {
        &PERMISSION_CREATE_PRIVATE_CHANNEL
    };
    if !state
        .app
        .session_has_permission_to_team(&session.0, &channel.team_id, permission)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[permission],
        )));
    }

    channel.creator_id.clone_from(&session.0.user_id);

    state.app.create_board_channel(&mut channel).await?;

    crate::commands::encoded(StatusCode::CREATED, &channel, "createBoard")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `null` decodes to the zero channel on Go's side, so it must here too — and the zero
    /// channel is refused as a `type`, not as a `body`.
    #[test]
    fn a_null_body_is_the_zero_channel() {
        let decoded = decode_one_from_json::<Option<Channel>>(b"null").unwrap();
        let channel = decoded.unwrap_or_default();
        assert!(!channel.is_board());
        assert!(decode_one_from_json::<Option<Channel>>(b"{not json").is_err());
    }
}
