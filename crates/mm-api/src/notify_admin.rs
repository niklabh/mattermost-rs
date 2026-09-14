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

/// Port of `handleTriggerNotifyAdminPosts` (api4/notify_admin.go:31) —
/// `POST /api/v4/users/trigger-notify-admin-posts`, an admin making the server send the
/// "please upgrade" posts the notify-admin rows are waiting for.
///
/// # The setting is the first check, before the body and before the caller
///
/// `ServiceSettings.EnableAPITriggerAdminNotifications` off — the default, and the stack — is
/// the 403 `api.cloud.app_error` for every request, session or not aside. On, the body is
/// `Decode(&ptr)` (a `null` or a non-object the 400 `notifyAdminRequest`), then `manage_system`,
/// then `SendNotifyAdminPosts`: the admins, the system bot, the pending rows, a DM post to each
/// admin and the rows stamped — forwarded.
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn handle_trigger_notify_admin_posts(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state.app.config().enable_api_trigger_admin_notifications {
        return ApiError::from(mm_model::utils::AppError::new(
            "Api4.handleTriggerNotifyAdminPosts",
            "api.cloud.app_error",
            None,
            "Manual triggering of notifications not allowed",
            403,
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let decoded = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value @ serde_json::Value::Object(_)) => {
            serde_json::from_value::<NotifyAdminToUpgradeRequest>(value).ok()
        }
        _ => None,
    };
    if decoded.is_none() {
        return ApiError::invalid_param("notifyAdminRequest").into_response();
    }

    // only system admins can manually trigger these notifications
    if !state
        .app
        .session_has_permission_to(&session.0, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*mm_model::permission::make_permission_error(
            &session.0,
            &[&mm_model::permission::PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    tracing::Span::current().record("forwarded", true);
    tracing::debug!("handing the notify-admin post batch to Go");
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    crate::proxy::forward_to_go(State(state), request).await
}
