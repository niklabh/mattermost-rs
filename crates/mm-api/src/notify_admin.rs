//! Port of `api4/notify_admin.go` — `POST /api/v4/users/notify-admin`, a non-admin asking the
//! admins to upgrade the workspace for a feature.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::notify_admin::NotifyAdminToUpgradeRequest;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::thread_writes::status_ok;

/// Port of `handleNotifyAdmin` (api4/notify_admin.go:13).
///
/// The body is decoded with Go's decoder semantics — `null` is a nil pointer and a 400 like a
/// malformed document, both `notifyAdminRequest` — through a `Value` round-trip, since serde
/// would accept `null` into a defaulted struct. The user is the session's; the body's fields
/// are the plan, the feature and the trial flag. Success is `ReturnStatusOK`.
#[tracing::instrument(skip_all)]
pub async fn handle_notify_admin(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("notifyAdminRequest").into_response();
        }
    };
    let decoded: Option<NotifyAdminToUpgradeRequest> =
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| match value {
                serde_json::Value::Null => None,
                serde_json::Value::Object(_) => serde_json::from_value(value).ok(),
                _ => None,
            });
    let Some(notify_admin_request) = decoded else {
        tracing::debug!("notify-admin body did not decode");
        return ApiError::invalid_param("notifyAdminRequest").into_response();
    };

    match state
        .app
        .save_admin_notification(&session.0.user_id, &notify_admin_request)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(*err).into_response(),
    }
}
