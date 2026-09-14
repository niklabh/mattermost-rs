//! Port of `testSiteURL` (api4/system.go) — `POST /api/v4/site_url/test`, the system console's
//! "test live URL" button.
//!
//! `test_site_url` behind `SessionHasPermissionToAndNotRestrictedAdmin` (so a restricted admin
//! is refused even with the permission), a `MapFromJSON` body whose `site_url` must be non-empty
//! (the 400 `site_url`), and then one plain `http.Get` of `{site_url}/api/v4/system/ping`
//! (app/admin.go:159): any transport error or any status but 200 is the 400
//! `app.admin.test_site_url.failure`. Redirects are followed, as Go's default client follows
//! them, and there is no timeout, as there is none in Go.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_TEST_SITE_URL, make_permission_error};
use mm_model::utils::{AppError, StringMap};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `testSiteURL` — `POST /api/v4/site_url/test`.
#[tracing::instrument(skip_all, fields(site_url, reachable))]
pub async fn test_site_url(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(&session.0, &PERMISSION_TEST_SITE_URL)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_TEST_SITE_URL],
        ))
        .into_response();
    }

    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    // `model.MapFromJSON` — every failure is an empty map.
    let props: StringMap = serde_json::from_slice(&bytes).unwrap_or_default();
    let site_url = props.get("site_url").map_or("", String::as_str);
    if site_url.is_empty() {
        return ApiError::invalid_param("site_url").into_response();
    }
    tracing::Span::current().record("site_url", site_url);

    // `http.Get(fmt.Sprintf("%s/api/v4/system/ping", siteURL))` — the URL is joined by string,
    // so a trailing slash on the setting doubles it, exactly as Go's does.
    let reachable = match state
        .http
        .get(format!("{site_url}/api/v4/system/ping"))
        .send()
        .await
    {
        Ok(response) => response.status() == reqwest::StatusCode::OK,
        Err(err) => {
            tracing::debug!(error = %err, "the site URL's ping did not answer");
            false
        }
    };
    tracing::Span::current().record("reachable", reachable);
    if !reachable {
        return ApiError::from(AppError::new(
            "testSiteURL",
            "app.admin.test_site_url.failure",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    // `ReturnStatusOK` — `{"status":"OK"}` with no trailing newline.
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}
