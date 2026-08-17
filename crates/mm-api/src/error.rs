//! The API edge's error type and its rendering.
//!
//! Go writes `model.AppError` as the response body for every failed API call, and clients parse
//! it: the webapp branches on `id`, and `status_code` appears in the body as well as the status
//! line. So the error shape is wire format, not diagnostics.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

/// An error on its way to a client, carrying the `AppError` Go would have written.
#[derive(Debug)]
pub struct ApiError(pub AppError);

impl ApiError {
    /// Port of the `ApiSessionRequired` rejection when no usable session is present.
    ///
    /// Go's id here is `api.context.session_expired.app_error` with `session_expired` as the
    /// detail — the same answer for a missing token and an expired one, which is deliberate on
    /// their side: it tells an attacker nothing about which tokens exist.
    pub fn unauthenticated() -> Self {
        ApiError(AppError::new(
            "ServeHTTP",
            "api.context.session_expired.app_error",
            None,
            "token not found or expired",
            401,
        ))
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // `AppError.status_code` is the authority; the status line and the body must agree,
        // because clients read the body's copy. An out-of-range or unset code becomes a 500
        // rather than a panic — `from_u16` is fallible and this is library code.
        let status = u16::try_from(self.0.status_code)
            .ok()
            .and_then(|code| StatusCode::from_u16(code).ok())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Serialising `AppError` cannot fail — every field is a string, an i32 or skipped — but
        // library code does not get to `unwrap`, so a failure degrades to a bare status.
        match serde_json::to_vec(&self.0) {
            Ok(body) => (
                status,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            Err(err) => {
                tracing::error!(error = %err, "failed to serialise AppError");
                status.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthenticated_is_401_with_gos_error_id() {
        let err = ApiError::unauthenticated();
        assert_eq!(err.0.status_code, 401);
        assert_eq!(err.0.id, "api.context.session_expired.app_error");
        assert_eq!(
            err.0.message, err.0.id,
            "untranslated message mirrors the id"
        );
    }

    #[test]
    fn the_status_line_follows_the_app_errors_code() {
        let response = ApiError::unauthenticated().into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A code outside the HTTP range must not take the process down.
    #[test]
    fn an_impossible_status_code_degrades_to_500() {
        let err = ApiError(AppError::new("X", "some.id", None, "", 9_999));
        assert_eq!(
            err.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// The body carries the same fields Go writes, including the duplicated status code.
    #[test]
    fn the_body_is_the_app_error_json() {
        let body = serde_json::to_value(ApiError::unauthenticated().0).expect("serialises");
        assert_eq!(body["id"], "api.context.session_expired.app_error");
        assert_eq!(body["status_code"], 401);
        assert!(body.get("detailed_error").is_some());
    }
}
