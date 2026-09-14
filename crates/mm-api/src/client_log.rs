//! Port of `postLog` (api4/system.go:59) — `POST /api/v4/logs`, a client writing a line into
//! the server's log. An `APIHandler`, so a session is optional — until the developer flag says
//! otherwise.
//!
//! # Who may log, and at what level
//!
//! With `ServiceSettings.EnableDeveloper` off (the default): no session is the 403
//! `api.context.permissions.app_error`, and a session without `manage_system` is accepted but
//! **forced to debug** — its `ERROR` lines are written at debug. With the flag on, anyone,
//! session or not, logs at the level they ask for. Only `level == "ERROR"` is an error line;
//! every other value, `error` included, is debug.
//!
//! # The body is a `map[string]string`, decoded the Go way
//!
//! `json.Decode` into a map creates the entry for every key **before** decoding its value, so
//! a key whose value is not a string is kept with the empty string and the error is only
//! logged — `{"level":"ERROR","message":1}` is `{"level":"ERROR","message":""}`, measured. serde
//! would drop the whole map; this reads the object itself and does what Go's decoder does. A
//! body that is not an object is an empty map. The message is cut to its
//! first **399 bytes** (`msg[0:399]`, a byte slice — a multibyte character on the boundary is
//! cut through, and Go's encoder then writes U+FFFD for the fragment, as `from_utf8_lossy`
//! does here), prefixed with `Client Logs API Endpoint Message: `, and the whole map is echoed
//! back with the rewritten `message` — `json.Encode`, keys sorted, trailing newline.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth_writes::OptionalSession;
use crate::error::ApiError;

/// `msg[0:399]` — the cut is at 399 bytes, not 400.
const MESSAGE_CUT: usize = 399;
const MESSAGE_PREFIX: &str = "Client Logs API Endpoint Message: ";

/// `json.NewDecoder(r.Body).Decode(&m)` into a `map[string]string`: every key of an object,
/// a non-string value as `""` — the entry exists before its value fails to decode — and
/// nothing for anything else.
fn string_map_from_json(bytes: &[u8]) -> std::collections::BTreeMap<String, String> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(fields)) => fields
            .into_iter()
            .map(|(key, value)| match value {
                serde_json::Value::String(value) => (key, value),
                _ => (key, String::new()),
            })
            .collect(),
        _ => std::collections::BTreeMap::new(),
    }
}

/// Port of `postLog` — `POST /api/v4/logs`.
#[tracing::instrument(skip_all, fields(level, forced_to_debug))]
pub async fn post_log(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    let session = session.0;
    let mut force_to_debug = false;
    if !state.app.config().enable_developer {
        let Some(session) = session.as_ref() else {
            return ApiError::from(AppError::new(
                "postLog",
                "api.context.permissions.app_error",
                None,
                String::new(),
                403,
            ))
            .into_response();
        };
        if !state
            .app
            .session_has_permission_to(session, &mm_model::permission::PERMISSION_MANAGE_SYSTEM)
            .await
        {
            force_to_debug = true;
        }
    }
    tracing::Span::current().record("forced_to_debug", force_to_debug);

    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let mut fields = string_map_from_json(&bytes);

    let level = fields.get("level").cloned().unwrap_or_default();
    tracing::Span::current().record("level", level.as_str());
    let raw = fields.get("message").map(String::as_str).unwrap_or("");
    let cut = if raw.len() > 400 {
        String::from_utf8_lossy(&raw.as_bytes()[..MESSAGE_CUT]).into_owned()
    } else {
        raw.to_owned()
    };
    let message = format!("{MESSAGE_PREFIX}{cut}");

    let (session_id, user_id) = session
        .as_ref()
        .map(|s| (s.id.as_str(), s.user_id.as_str()))
        .unwrap_or(("", ""));
    if !force_to_debug && level == "ERROR" {
        tracing::error!(
            r#type = "client_message",
            user_agent = %user_agent,
            session_id = %session_id,
            user_id = %user_id,
            "{message}"
        );
    } else {
        tracing::debug!(
            r#type = "client_message",
            user_agent = %user_agent,
            session_id = %session_id,
            user_id = %user_id,
            "{message}"
        );
    }
    fields.insert("message".to_owned(), message);

    let mut body = match serde_json::to_vec(&fields) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to encode the client log echo");
            return ApiError::from(AppError::new(
                "postLog",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::string_map_from_json;

    /// The map keeps every key, a non-string value as `""` — Go's decoder creates the entry
    /// before the value fails — and anything but an object is empty.
    #[test]
    fn the_map_decodes_like_gos_string_map() {
        let partial = string_map_from_json(br#"{"level":"ERROR","message":1,"extra":"x"}"#);
        assert_eq!(partial.get("level").map(String::as_str), Some("ERROR"));
        assert_eq!(partial.get("message").map(String::as_str), Some(""));
        assert_eq!(partial.get("extra").map(String::as_str), Some("x"));
        for bad in [&b"null"[..], b"[]", b"\"x\"", b"", b"{"] {
            assert!(
                string_map_from_json(bad).is_empty(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
