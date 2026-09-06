//! Port of `getDirectOrGroupMessageMembersCommonTeams` (channels/api4/channel.go:3154), reached
//! as `GET /api/v4/channels/{channel_id}/common_teams`.
//!
//! The webapp asks this before offering to convert a group message into a channel: the answer is
//! the list of teams the conversion could target. Nothing else calls it.
//!
//! # A channel that does not exist is a **403**, not a 404
//!
//! Nothing looks the channel up before the permission check, and
//! `SessionHasPermissionToChannel` answers `false` when it cannot fetch the channel — so an
//! unknown id is "you may not read that", which is also what it tells someone asking about a
//! channel they are not in. That is the same answer for the same reason: this route must not say
//! which DMs exist.
//!
//! # Two empty answers, and they are different bytes
//!
//! `null` when the caller is not an active member of the channel (Go returns a nil slice on
//! purpose, channel.go:4275) and `[]` when there is simply no team in common. See
//! [`mm_app::common_teams::CommonTeams`].

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::common_teams::CommonTeams;
use mm_model::permission::{PERMISSION_READ_CHANNEL, make_permission_error};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `getDirectOrGroupMessageMembersCommonTeams` (channel.go:3154).
///
/// # The guest check comes first, and it is not a permission error
///
/// Before any permission question, the handler fetches the **caller** and refuses a guest with
/// `api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error` — a 403 with an id
/// that names *conversion*, on a route that only reads. Ordering it after the permission check
/// would change which of two 403s a guest outside the channel receives.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(SanitizeTeams(...))` — a trailing newline, and the teams are
/// sanitised per team against the caller's `manage_team` / `invite_user`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, outcome))]
pub async fn get_common_teams(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match serve(&state, &channel_id, &session).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => proxy::forward_to_go(State(state), request).await,
    }
}

enum Outcome {
    Served(Response),
    Failed(ApiError),
    Forward,
}

async fn serve(state: &AppState, channel_id: &str, session: &AuthenticatedSession) -> Outcome {
    if !is_valid_id(channel_id) {
        return Outcome::Failed(ApiError::invalid_url_param("channel_id"));
    }

    let user = match state.app.get_user(&session.0.user_id).await {
        Ok(user) => user,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };
    if user.is_guest() {
        tracing::Span::current().record("outcome", "guest");
        return Outcome::Failed(ApiError::from(AppError::new(
            "Api4.getDirectOrGroupMessageMembersCommonTeams",
            "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
            None,
            format!("userId={}", session.0.user_id),
            403,
        )));
    }

    let (allowed, _) = state
        .app
        .session_has_permission_to_channel(&session.0, channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    if !allowed {
        tracing::Span::current().record("outcome", "refused");
        return Outcome::Failed(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL],
        )));
    }

    let common = match state
        .app
        .get_direct_or_group_message_members_common_teams_as_user(&session.0.user_id, channel_id)
        .await
    {
        Ok(common) => common,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    let body = match common {
        CommonTeams::BotMember => {
            tracing::Span::current().record("outcome", "bot member; forwarding");
            return Outcome::Forward;
        }
        CommonTeams::NotAMember => {
            tracing::Span::current().record("outcome", "not a member");
            // Go's nil slice through `SanitizeTeams`, which returns it unchanged.
            b"null".to_vec()
        }
        CommonTeams::Teams(mut teams) => {
            tracing::Span::current().record("outcome", "teams");
            state.app.sanitize_teams(&session.0, &mut teams).await;
            match serde_json::to_vec(&teams) {
                Ok(body) => body,
                Err(err) => {
                    tracing::error!(error = %err, "failed to serialise the common teams");
                    return Outcome::Failed(ApiError::from(AppError::new(
                        "getDirectOrGroupMessageMembersCommonTeams",
                        "api.marshal_error",
                        None,
                        String::new(),
                        500,
                    )));
                }
            }
        }
    };

    let mut body = body;
    body.push(b'\n');
    Outcome::Served(
        (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}
