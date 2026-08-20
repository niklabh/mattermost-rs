//! The API edge's error type and its rendering.
//!
//! Go writes `model.AppError` as the response body for every failed API call, and clients parse
//! it: the webapp branches on `id`, and `status_code` appears in the body as well as the status
//! line. So the error shape is wire format, not diagnostics.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

/// The cutover marker every response this server produces carries: `rust` when a migrated handler
/// answered, `go` when the proxy forwarded. Declared here because errors need it as much as
/// successes do.
pub const SERVED_BY: axum::http::HeaderName =
    axum::http::HeaderName::from_static("x-mmrs-served-by");

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

    /// Port of `NewInvalidParamError` (web/context.go:254) — the answer for a body that will not
    /// decode or fails a bounds check. 400, with the parameter name in the params map.
    pub fn invalid_param(parameter: &str) -> Self {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "Name".to_owned(),
            serde_json::Value::String(parameter.to_owned()),
        );
        ApiError(AppError::new(
            "Context",
            "api.context.invalid_body_param.app_error",
            Some(params),
            String::new(),
            400,
        ))
    }

    /// Port of `NewInvalidURLParamError` (web/context.go:259) — a path segment that is not a
    /// valid id. 400, and note the **id differs from the body-param one by a single word**:
    /// `invalid_url_param` versus `invalid_body_param`. Both carry the parameter name as `Name`.
    pub fn invalid_url_param(parameter: &str) -> Self {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "Name".to_owned(),
            serde_json::Value::String(parameter.to_owned()),
        );
        ApiError(AppError::new(
            "Context",
            "api.context.invalid_url_param.app_error",
            Some(params),
            String::new(),
            400,
        ))
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(mut self) -> Response {
        // Port of the tail of `web.Handler.ServeHTTP` (channels/web/handlers.go:424-455), which
        // is where Go turns an `AppError` into a body. Three steps happen there and nowhere else,
        // so an `AppError` serialised straight out of a handler is NOT what a client sees:
        //
        //   1. `c.Err.RequestId = c.AppContext.RequestId()` — populated on every error.
        //   2. `c.Err.Translate(c.AppContext.T)` — the id becomes a human message. Not ported;
        //      we emit the untranslated id, which is what an unconfigured Go server also does.
        //      See [D-092].
        //   3. `if !EnableDeveloper { c.Err.WipeDetailed() }` — `detailed_error` is blanked. The
        //      setting defaults to false, so **the default is to wipe**, and a port that skips
        //      this leaks internal detail Go withholds. Reproduced unconditionally because the
        //      config that would turn it off is not ported either.
        self.0.request_id = mm_model::utils::new_id();
        self.0.wipe_detailed();

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
                [
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                    // Our own cutover marker, matching what every success path and the proxy set.
                    // Errors had been omitting it, which meant a 403 from a migrated route and a
                    // 403 forwarded to Go were indistinguishable — including to the parity suite,
                    // whose "was this actually served by Rust" guard reads exactly this header.
                    (SERVED_BY, "rust"),
                ],
                body,
            )
                .into_response(),
            Err(err) => {
                tracing::error!(error = %err, "failed to serialise AppError");
                (status, [(SERVED_BY, "rust")]).into_response()
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

    /// An error is marked as ours. Without this a refusal from a migrated route looks exactly
    /// like one forwarded to Go, which is what let a whole parity suite pass while every request
    /// was being proxied.
    #[test]
    fn an_error_response_is_marked_as_served_by_rust() {
        let response = ApiError::unauthenticated().into_response();
        assert_eq!(
            response
                .headers()
                .get(SERVED_BY)
                .and_then(|v| v.to_str().ok()),
            Some("rust")
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
