//! Port of `pushNotificationAck` (api4/system.go) — `POST /api/v4/notifications/ack`, a mobile
//! device confirming a push notification.
//!
//! # The decode, then the setting
//!
//! The body is `json.NewDecoder(r.Body).Decode(&ack)` into a **value**, so a `null` is a
//! no-op that leaves every field zero and only a non-object or a mistyped field is the 400
//! `api.push_notifications_ack.message.parse.app_error`. Then `EmailSettings.SendPushNotifications`:
//! off, the 501 `api.push_notification.disabled.app_error`.
//!
//! # Past the setting, three answers
//!
//! A `message` ack from an iOS session whose device notifications were marked disabled
//! re-enables them (iOS sends no ack while they are off). The ack is then posted to the push
//! proxy ([`mm_app::App::send_ack_to_push_proxy`]), and:
//!
//! - an ordinary ack answers `{"status":"OK"}`, or the 500
//!   `api.push_notifications_ack.forward.app_error` when the proxy could not be told;
//! - an **id-loaded** ack never fails on the proxy — the error is only logged — and answers an
//!   **empty 200** (`return` with nothing written) unless it is a `message` ack with a post id;
//! - that last case reads the post through `GetPostIfAuthorized` (its 403/404 are the answer) and
//!   then needs `App.Notification()`, the Enterprise id-loaded interface, which is nil on every
//!   build we run: Go answers **302** `api.system.id_loaded.not_available.app_error`, and so does
//!   this.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::push_notification::PushNotificationAck;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `pushNotificationAck` — `POST /api/v4/notifications/ack`.
#[tracing::instrument(skip_all)]
pub async fn push_notification_ack(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();

    // `Decode(&ack)` into a struct value: `null` decodes to nothing, an object to the fields —
    // a mistyped one an error — and anything else is an error.
    let decoded = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Null) => Ok(PushNotificationAck::default()),
        Ok(value @ serde_json::Value::Object(_)) => {
            serde_json::from_value::<PushNotificationAck>(value).map_err(|err| err.to_string())
        }
        Ok(_) => Err("not an object".to_owned()),
        Err(err) => Err(err.to_string()),
    };
    if let Err(details) = decoded {
        return ApiError::from(AppError::new(
            "pushNotificationAck",
            "api.push_notifications_ack.message.parse.app_error",
            None,
            details,
            400,
        ))
        .into_response();
    }

    if !state.app.config().send_push_notifications {
        return ApiError::from(AppError::new(
            "pushNotificationAck",
            "api.push_notification.disabled.app_error",
            None,
            String::new(),
            501,
        ))
        .into_response();
    }

    let ack = decoded.unwrap_or_default();

    if mm_app::App::is_message_push(&ack.notification_type) {
        let mut session = session.0;
        let ignore = session
            .props
            .as_ref()
            .and_then(|props| props.get("device_notification_disabled"))
            .is_some_and(|value| value == "true");
        if ignore && ack.client_platform == "ios" {
            if let Err(err) = state
                .app
                .set_extra_session_props(&mut session, &[("device_notification_disabled", "false")])
                .await
            {
                tracing::warn!(error = %err.id, "Failed to set extra session props");
            }
            state
                .app
                .clear_session_cache_for_user(&session.user_id)
                .await;
        }
        return finish(&state, ack, Some(session), parts, bytes).await;
    }
    finish(&state, ack, Some(session.0), parts, bytes).await
}

async fn finish(
    state: &AppState,
    ack: PushNotificationAck,
    session: Option<mm_model::session::Session>,
    parts: axum::http::request::Parts,
    _bytes: axum::body::Bytes,
) -> Response {
    let sent = state.app.send_ack_to_push_proxy(&ack).await;
    if ack.is_id_loaded {
        if let Err(err) = &sent {
            tracing::error!(error = %err, "Notification ack not sent to push proxy");
        }
        if !ack.post_id.is_empty() && mm_app::App::is_message_push(&ack.notification_type) {
            let Some(session) = session else {
                return StatusCode::OK.into_response();
            };
            let request = Request::from_parts(parts, axum::body::Body::empty());
            let hook_ctx = crate::plugin_context::hook_context_of(&request, Some(&session));
            if let Err(err) = state
                .app
                .get_post_if_authorized(&hook_ctx, &ack.post_id, &session, false)
                .await
            {
                return ApiError::from(*err).into_response();
            }
            return ApiError::from(AppError::new(
                "pushNotificationAck",
                "api.system.id_loaded.not_available.app_error",
                None,
                String::new(),
                302,
            ))
            .into_response();
        }
        return (StatusCode::OK, [("x-mmrs-served-by", "rust")]).into_response();
    }
    if let Err(err) = sent {
        tracing::error!(error = %err, "sending the ack to the push proxy failed");
        return ApiError::from(AppError::new(
            "pushNotificationAck",
            "api.push_notifications_ack.forward.app_error",
            None,
            String::new(),
            500,
        ))
        .into_response();
    }
    crate::user_creates::status_ok()
}
