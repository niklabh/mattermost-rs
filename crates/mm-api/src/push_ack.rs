//! Port of `pushNotificationAck` (api4/system.go) — `POST /api/v4/notifications/ack`, a mobile
//! device confirming a push notification.
//!
//! # Served up to the setting, forwarded past it
//!
//! The body is `json.NewDecoder(r.Body).Decode(&ack)` into a **value**, so a `null` is a
//! no-op that leaves every field zero and only a non-object or a mistyped field is the 400
//! `api.push_notifications_ack.message.parse.app_error`. Then `EmailSettings.SendPushNotifications`:
//! off, the 501 `api.push_notification.disabled.app_error` — which is the whole route on a
//! server without a push proxy. On, Go counts the ack, may clear the session's
//! `DeviceNotificationDisabled` prop, sends the ack to the push proxy over HTTP and, for an
//! id-loaded `message` ack with a post id, answers the notification's own message — all of
//! which is forwarded.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::push_notification::PushNotificationAck;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `pushNotificationAck` — `POST /api/v4/notifications/ack`.
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn push_notification_ack(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
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

    tracing::Span::current().record("forwarded", true);
    tracing::debug!("handing a push notification ack to Go's push proxy client");
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    proxy::forward_to_go(State(state), request).await
}
