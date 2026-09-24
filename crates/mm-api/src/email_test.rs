//! Port of `testEmail` (api4/system.go:243) — `POST /api/v4/email/test`, the system console's
//! "send a test email" button, served through to the SMTP send ([`mm_app::App::test_email`]).
//!
//! # The nil-field check comes before the permission
//!
//! The body is decoded as a whole `model.Config`; `checkHasNilFields(&cfg.EmailSettings)` then
//! refuses with the 400 `api.file.test_connection_email_settings_nil.app_error` if **any** of
//! the section's thirty pointer fields is absent — so a plain member sending a partial section
//! learns that before being refused for `test_email`.
//!
//! # When Go tests its live configuration instead
//!
//! `json.NewDecoder(r.Body).Decode(&cfg)` into a nil `*model.Config` leaves it nil for an empty
//! body, for `null`, and for any syntax error — the decoder scans the whole first value before it
//! unmarshals a byte of it — and `cfg == nil` means "use `c.App.Config()`". Those are served here
//! from the live configuration. A first value that is valid JSON but not an object (`[]`, `"x"`)
//! **does** allocate the struct before the type error, so Go goes on with a zero config — that,
//! and an object serde will not decode where Go fills the struct partway before failing, are
//! forwarded rather than guessed at.
//!
//! # The send ignores the body's server
//!
//! Past the permission, the body is only checked — an empty `SMTPServer`, or a masked password
//! for different connection settings — and the mail then goes through the **live** SMTP
//! settings; see [`mm_app::App::test_email`].

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

    // `Decoder.Decode` reads the *first* value only, so trailing bytes after it are ignored.
    let first = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<serde_json::Value>()
        .next();
    let forward = |state: AppState, why: &'static str| async move {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!(reason = why, "handing an email test to Go");
        let request = Request::from_parts(parts, axum::body::Body::from(bytes));
        proxy::forward_to_go(State(state), request).await
    };

    let email_settings = match first {
        Some(Ok(value @ serde_json::Value::Object(_))) => {
            match serde_json::from_value::<Config>(value) {
                Ok(cfg) => cfg.email_settings,
                Err(_) => return forward(state, "a body Go decodes partially").await,
            }
        }
        // EOF, `null`, or a syntax error: `cfg` stays nil and Go tests its live configuration.
        None | Some(Ok(serde_json::Value::Null)) | Some(Err(_)) => {
            match mm_app::config::load_model_config(state.app.store().config()).await {
                Ok(live) => live.email_settings,
                Err(err) => {
                    tracing::error!(error = %err, "could not read the live configuration");
                    return ApiError::from(AppError::new(
                        "testEmail",
                        "app.admin.test_email.failure",
                        None,
                        String::new(),
                        500,
                    ))
                    .into_response();
                }
            }
        }
        Some(Ok(_)) => return forward(state, "a non-object body Go zero-fills").await,
    };

    if email_settings_have_nil_fields(&email_settings) {
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

    match state
        .app
        .test_email(&session.0.user_id, &email_settings)
        .await
    {
        Ok(()) => crate::user_creates::status_ok(),
        Err(err) => ApiError::from(*err).into_response(),
    }
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
