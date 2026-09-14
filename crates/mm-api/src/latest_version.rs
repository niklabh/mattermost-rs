//! Port of `getLatestVersion` (api4/system.go:527) — `GET /api/v4/latest_version`, the console's
//! "is there a newer release" lookup against GitHub.
//!
//! `manage_system` **and not a restricted admin** (`SessionHasPermissionToAndNotRestrictedAdmin`
//! — with `ExperimentalSettings.RestrictSystemAdmin` on, even the admin is refused), then
//! [`mm_app::App::get_latest_version`] against the one constant URL, and the release
//! `json.Marshal`ed — Go's HTML-escaping encoder, no trailing newline. The marshal-failure 500
//! is unreachable for a value that was just decoded.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// The literal in `getLatestVersion` (system.go:533). Still the pre-monorepo repository name.
pub const LATEST_VERSION_URL: &str =
    "https://api.github.com/repos/mattermost/mattermost-server/releases/latest";

/// Port of `getLatestVersion` — `GET /api/v4/latest_version`.
#[tracing::instrument(skip_all)]
pub async fn get_latest_version(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let release = state.app.get_latest_version(LATEST_VERSION_URL).await?;
    let body = mm_model::utils::go_json_marshal(&release).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the release");
        ApiError::from(AppError::new(
            "getLatestVersion",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}
