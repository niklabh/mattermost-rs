//! Port of `testFileStore` (api4/system.go) — `POST /api/v4/file/test` and its older alias
//! `POST /api/v4/file/s3_test`, the system console's "test file storage connection".
//!
//! # The permission comes before the nil-field check here
//!
//! The opposite order from `testEmail`: `test_s3` first (kept for every backend, the comment in
//! Go says, to avoid a role migration), then `checkHasNilFields(&cfg.FileSettings)` over the
//! section's sixty-three pointer fields — the 400 `api.file.test_connection_settings_nil` —
//! then the driver: `local` needs nothing, `amazons3` a bucket (400
//! `api.admin.test_s3.missing_s3_bucket`), `azureblob` an account, a container and, for the
//! shared-key mode, a key (400 `api.admin.test_azure.missing_azure_field`), and anything else
//! the 400 `api.file.test_connection_unsupported_driver`.
//!
//! # What is served past the checks, and what is not
//!
//! `local` is tested here: a backend over the body's `Directory`, and its `TestConnection` —
//! `testfile` written and removed — whose failure is the 500 `api.file.test_connection`. The
//! directory is resolved against this process's working directory, as Go's is against its own,
//! so a relative path names two different places on the two servers; the suite uses absolute
//! ones. `amazons3` and `azure` — `Desanitize` swapping the fake secret back and the SDK
//! connection — are forwarded, as is a body that does not decode (Go tests its live config).

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::filestore::{FileBackend, FileBackendSettings};
use mm_model::config::{Config, FileSettings};
use mm_model::permission::{PERMISSION_TEST_S3, make_permission_error};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `checkHasNilFields(&cfg.FileSettings)`: any of the section's `Option`s `None`, read off the
/// serialised form like the email one.
pub(crate) fn file_settings_have_nil_fields(settings: &FileSettings) -> bool {
    match serde_json::to_value(settings) {
        Ok(serde_json::Value::Object(fields)) => fields.values().any(serde_json::Value::is_null),
        _ => true,
    }
}

fn refusal(where_: &str, id: &str, details: &str, status: i32) -> Response {
    ApiError::from(AppError::new(where_, id, None, details, status)).into_response()
}

/// Port of `testFileStore` — `POST /api/v4/file/test` and `POST /api/v4/file/s3_test`.
#[tracing::instrument(skip_all, fields(driver, forwarded = false))]
pub async fn test_file_store(
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
        tracing::debug!(reason = why, "handing a file-store test to Go");
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

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_TEST_S3)
        .await
    {
        return ApiError::from(*make_permission_error(&session.0, &[&PERMISSION_TEST_S3]))
            .into_response();
    }

    if file_settings_have_nil_fields(&cfg.file_settings) {
        return refusal(
            "testFileStore",
            "api.file.test_connection_settings_nil.app_error",
            "",
            400,
        );
    }

    let settings = &cfg.file_settings;
    let driver = settings.driver_name.as_deref().unwrap_or("");
    tracing::Span::current().record("driver", driver);
    match driver {
        "local" => {}
        "amazons3" => {
            if settings
                .amazon_s3_bucket
                .as_deref()
                .unwrap_or("")
                .is_empty()
            {
                return refusal(
                    "CheckMandatoryS3Fields",
                    "api.admin.test_s3.missing_s3_bucket",
                    "",
                    400,
                );
            }
            return forward(state, "the S3 connection").await;
        }
        "azureblob" => {
            if settings
                .azure_storage_account
                .as_deref()
                .unwrap_or("")
                .is_empty()
            {
                return refusal(
                    "CheckMandatoryAzureFields",
                    "api.admin.test_azure.missing_azure_field",
                    "missing azure storage account setting",
                    400,
                );
            }
            if settings.azure_container.as_deref().unwrap_or("").is_empty() {
                return refusal(
                    "CheckMandatoryAzureFields",
                    "api.admin.test_azure.missing_azure_field",
                    "missing azure container setting",
                    400,
                );
            }
            if settings.azure_auth_mode.as_deref() == Some("shared_key")
                && settings
                    .azure_access_key
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
            {
                return refusal(
                    "CheckMandatoryAzureFields",
                    "api.admin.test_azure.missing_azure_field",
                    "missing azure access key setting",
                    400,
                );
            }
            return forward(state, "the Azure connection").await;
        }
        _ => {
            let params = std::collections::HashMap::from([(
                "Driver".to_owned(),
                serde_json::Value::String(driver.to_owned()),
            )]);
            return ApiError::from(AppError::new(
                "testFileStore",
                "api.file.test_connection_unsupported_driver.app_error",
                Some(params),
                String::new(),
                400,
            ))
            .into_response();
        }
    }

    // `TestFileStoreConnectionWithConfig` for `local`: a backend over the body's directory and
    // its `TestConnection`.
    let directory = settings.directory.as_deref().unwrap_or("");
    let backend = FileBackend::new(&FileBackendSettings::from_file_settings("local", directory));
    if let Err(err) = backend.test_connection().await {
        tracing::debug!(error = %err, "the local file store test failed");
        return refusal(
            "TestConnection",
            "api.file.test_connection.app_error",
            &err.to_string(),
            500,
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The default section — every field `None` — has nil fields; the fixture's has none.
    #[test]
    fn the_nil_check_reads_all_sixty_three_fields() {
        let empty = FileSettings::default();
        let fields = serde_json::to_value(&empty).expect("serialises");
        assert_eq!(fields.as_object().map(|o| o.len()), Some(63));
        assert!(file_settings_have_nil_fields(&empty));

        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/config.json")).expect("JSON");
        let full: FileSettings =
            serde_json::from_value(fixture["FileSettings"].clone()).expect("decodes");
        assert!(!file_settings_have_nil_fields(&full));
    }
}
