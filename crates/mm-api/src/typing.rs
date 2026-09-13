//! Port of `publishUserTyping` (api4/user.go:3599), registered at user.go:101 as
//! `POST /api/v4/users/{user_id}/typing` with `APISessionRequiredDisableWhenBusy`.
//!
//! The REST half of "user is typing". The websocket action of the same name (`user_typing` on
//! the socket router) is a separate, still-unported surface — see the hub entry in
//! `docs/TECH_DEBT.md` — and shares only `App::publish_user_typing` with this one.

use axum::extract::{Path, Request, State};
use axum::response::Response;
use mm_model::permission::{
    PERMISSION_CREATE_POST, PERMISSION_MANAGE_SYSTEM, make_permission_error,
};
use mm_model::typing_request::TypingRequest;
use mm_model::utils::{decode_one_from_json, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{read_body, resolve_me};
use crate::error::ApiError;
use crate::system::refuse_when_busy;
use crate::thread_writes::status_ok;

/// Port of `publishUserTyping` (api4/user.go:3599).
///
/// # Four refusals, in this order
///
/// 1. The busy gate (`DisableWhenBusy`), before the handler body: 503.
/// 2. `RequireUserId` — `me` resolves first, then the 26-character check: 400 `user_id`.
/// 3. The body: `json.NewDecoder(r.Body).Decode` into `TypingRequest`, and any failure is
///    `SetInvalidParamWithErr("typing_request", …)` — 400, **before** any permission is asked,
///    so a malformed body from a caller who could never type here is still a 400. A JSON
///    `null` is not a failure: it is the zero request, refused two steps later as `create_post`.
/// 4. Typing *as* someone else needs `manage_system`; then whoever is typing needs
///    `create_post` in the channel, via `HasPermissionToChannel` — which is `(false, false)` for
///    an empty or unknown `channel_id`, so a missing channel is a 403 naming `create_post`, not
///    a 400.
///
/// The success body is `ReturnStatusOK`, newline-free.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn publish_user_typing(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    refuse_when_busy()?;

    let user_id = resolve_me(&user_id, &session);
    if !is_valid_id(user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    let body = read_body(request, "publishUserTyping").await?;
    // `json.NewDecoder(r.Body).Decode(&typingRequest)` into a struct: `null` is accepted and
    // leaves the zero value (so an empty `channel_id`, and the 403 below), while an array is
    // "cannot unmarshal array into Go value" — which serde's derive would have accepted
    // positionally. The `Value` round trip is the same one `viewChannel` and the token routes
    // use for the same two reasons; a lone surrogate is folded by `decode_one_from_json`.
    let decoded: Option<serde_json::Value> = decode_one_from_json(&body).map_err(|err| {
        tracing::debug!(error = %err, "the typing request body does not decode");
        ApiError::invalid_param("typing_request")
    })?;
    let typing: TypingRequest = match decoded {
        None => TypingRequest::default(),
        Some(value @ serde_json::Value::Object(_)) => {
            serde_json::from_value(value).map_err(|err| {
                tracing::debug!(error = %err, "the typing request has the wrong field types");
                ApiError::invalid_param("typing_request")
            })?
        }
        Some(_) => return Err(ApiError::invalid_param("typing_request")),
    };

    if user_id != session.0.user_id
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let (allowed, _is_member) = state
        .app
        .has_permission_to_channel(user_id, &typing.channel_id, &PERMISSION_CREATE_POST)
        .await;
    if !allowed {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_POST],
        )));
    }

    state
        .app
        .publish_user_typing(user_id, &typing.channel_id, &typing.parent_id)
        .await?;

    Ok(status_ok())
}
