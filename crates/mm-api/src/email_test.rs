//! Port of `testEmail` (api4/system.go) — `POST /api/v4/email/test`, the system console's
//! "send a test email" button, served up to the SMTP send.
//!
//! # The nil-field check comes before the permission
//!
//! The body is decoded as a whole `model.Config`; `checkHasNilFields(&cfg.EmailSettings)` then
//! refuses with the 400 `api.file.test_connection_email_settings_nil.app_error` if **any** of
//! the section's thirty pointer fields is absent — so a plain member sending a partial section
//! learns that before being refused for `test_email`. A body that does not decode leaves `cfg`
//! nil and Go substitutes its **live** configuration, which passes the check; that arm, and
//! everything past the empty-`SMTPServer` refusal (the fake-password swap, the user's locale,
//! the SMTP send), is forwarded.
//!
//! `json.Decode` fills the struct before it fails, so a body with a mistyped field is a
//! partially filled section on Go's side — usually the nil-field 400 — where serde drops the
//! whole thing; that difference is forwarded too rather than guessed at.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::config::{Config, EmailSettings};
use mm_model::permission::{PERMISSION_TEST_EMAIL, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `checkHasNilFields(&cfg.EmailSettings)` (api4/system.go:1121): true when any pointer field
/// of the section is nil — here, any of its `Option`s `None`, read off the serialised form so
/// that a field added to the model is checked without this list being touched.
pub(crate) fn email_settings_have_nil_fields(settings: &EmailSettings) -> bool {
    match serde_json::to_value(settings) {
        Ok(serde_json::Value::Object(fields)) => fields.values().any(serde_json::Value::is_null),
        _ => true,
    }
}

/// Port of `testEmail` — `POST /api/v4/email/test`.
#[tracing::instrument(skip_all, fields(forwarded = false))]
pub async fn test_email(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();

    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
    let forward = |state: AppState, why: &'static str| async move {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!(reason = why, "handing an email test to Go");
        let request = Request::from_parts(parts, axum::body::Body::from(bytes));
        proxy::forward_to_go(State(state), request).await
    };

    let cfg = match parsed {
        Ok(value @ serde_json::Value::Object(_)) => match serde_json::from_value::<Config>(value) {
            Ok(cfg) => cfg,
            Err(_) => return forward(state, "a body Go decodes partially").await,
        },
        _ => return forward(state, "no config in the body: Go tests its live one").await,
    };

    if email_settings_have_nil_fields(&cfg.email_settings) {
        return ApiError::from(AppError::new(
            "testEmail",
            "api.file.test_connection_email_settings_nil.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_and_not_restricted_admin(&session.0, &PERMISSION_TEST_EMAIL)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_TEST_EMAIL],
        ))
        .into_response();
    }

    // `TestEmail`'s first statement; its detail is the translated `invalid_param` sentence for
    // `SMTPServer`, blanked on the wire.
    if cfg
        .email_settings
        .smtp_server
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        return ApiError::from(AppError::new(
            "testEmail",
            "api.admin.test_email.missing_server",
            None,
            "Invalid or missing SMTPServer parameter in request body.",
            400,
        ))
        .into_response();
    }

    forward(state, "the SMTP send").await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default section — every field `None` — is thirty nulls on the wire and so has nil
    /// fields; one with every field set has none. This is what pins the check to the model's
    /// thirty `Option`s rather than to a hand-kept list.
    #[test]
    fn the_nil_check_reads_all_thirty_fields() {
        let empty = EmailSettings::default();
        let fields = serde_json::to_value(&empty).expect("serialises");
        assert_eq!(fields.as_object().map(|o| o.len()), Some(30));
        assert!(email_settings_have_nil_fields(&empty));

        let full: EmailSettings = serde_json::from_str(
            &serde_json::to_string(
                &serde_json::from_str::<serde_json::Value>(include_str!(
                    "../../../fixtures/config.json"
                ))
                .expect("the fixture is JSON")["EmailSettings"],
            )
            .expect("re-serialises"),
        )
        .expect("the fixture section decodes");
        assert!(!email_settings_have_nil_fields(&full));
    }
}
