//! Port of `api4/compliance.go` — all four routes.
//!
//! # The refusal comes from the app layer, so the handler's own checks run first
//!
//! `App.SaveComplianceReport` and friends answer `ent.compliance.licence_disable.app_error` at
//! **501** when the compliance interface is absent, which on the Team Edition binary is always.
//! But every handler does its own work before reaching them, and — as in `/data_retention` — the
//! order differs per route:
//!
//! | Route | Order |
//! |---|---|
//! | `createComplianceReport` | body, then `create_compliance_export_job` |
//! | `getComplianceReports` | `read_compliance_export_job` |
//! | `getComplianceReport` | the report id, then `read_compliance_export_job` |
//! | `downloadComplianceReport` | the report id, then **`download_compliance_export_result`** |
//!
//! Three different permissions across four routes, and the download's is not the read one its
//! sibling uses.
//!
//! # `RequireReportId` **is** `IsValidId`, and its result **is** checked
//!
//! Unlike `RequirePolicyId` in `/data_retention` — whose 400 the app error overwrites — both
//! routes here test `c.Err` and return, so a malformed report id really is a 400. Two files, two
//! conventions; the parity suite asserts this one rather than assuming it from the neighbour.

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::permission::Permission;
use mm_model::permission::{
    PERMISSION_CREATE_COMPLIANCE_EXPORT_JOB, PERMISSION_DOWNLOAD_COMPLIANCE_EXPORT_RESULT,
    PERMISSION_READ_COMPLIANCE_EXPORT_JOB, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::LicenceGate;
use crate::error::ApiError;

/// `newLicenseError` for compliance (app/compliance.go).
const COMPLIANCE_LICENCE_DISABLED: &str = "ent.compliance.licence_disable.app_error";

/// The 501, or the proxy when a licence is installed.
async fn refuse_or_forward(state: &AppState, where_: &'static str, request: Request) -> Response {
    match crate::channels::licence_gate(state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            where_,
            COMPLIANCE_LICENCE_DISABLED,
            None,
            String::new(),
            501,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

async fn require(
    state: &AppState,
    session: &mm_model::session::Session,
    permission: &'static Permission,
) -> Result<(), ApiError> {
    if state
        .app
        .session_has_permission_to(session, permission)
        .await
    {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        session,
        &[permission],
    )))
}

/// Port of `createComplianceReport` (compliance.go:24).
///
/// The body is decoded first, so a malformed one is a 400 even for a caller with no permission.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_compliance_report(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return ApiError::invalid_param("compliance").into_response(),
    };
    // `json.NewDecoder(r.Body).Decode(&model.Compliance)` — every field optional to Go's decoder,
    // so only malformed JSON fails.
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return ApiError::invalid_param("compliance").into_response();
    }

    if let Err(err) = require(&state, &session.0, &PERMISSION_CREATE_COMPLIANCE_EXPORT_JOB).await {
        return err.into_response();
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    refuse_or_forward(&state, "createComplianceReport", request).await
}

/// Port of `getComplianceReports` (compliance.go:61).
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_compliance_reports(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require(&state, &session.0, &PERMISSION_READ_COMPLIANCE_EXPORT_JOB).await {
        return err.into_response();
    }
    refuse_or_forward(&state, "getComplianceReports", request).await
}

/// Port of `getComplianceReport` (compliance.go:82) — the id, **then** the permission.
#[tracing::instrument(skip_all, fields(report_id, licensed))]
pub async fn get_compliance_report(
    State(state): State<AppState>,
    Path(report_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("report_id", &report_id);
    if !is_valid_id(&report_id) {
        return ApiError::invalid_url_param("report_id").into_response();
    }
    if let Err(err) = require(&state, &session.0, &PERMISSION_READ_COMPLIANCE_EXPORT_JOB).await {
        return err.into_response();
    }
    refuse_or_forward(&state, "getComplianceReport", request).await
}

/// Port of `downloadComplianceReport` (compliance.go:117).
///
/// **A third permission.** `download_compliance_export_result`, not the `read_...` its sibling
/// uses on the same resource — so a role that may list and read reports may still not download
/// one.
#[tracing::instrument(skip_all, fields(report_id, licensed))]
pub async fn download_compliance_report(
    State(state): State<AppState>,
    Path(report_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("report_id", &report_id);
    if !is_valid_id(&report_id) {
        return ApiError::invalid_url_param("report_id").into_response();
    }
    if let Err(err) = require(
        &state,
        &session.0,
        &PERMISSION_DOWNLOAD_COMPLIANCE_EXPORT_RESULT,
    )
    .await
    {
        return err.into_response();
    }
    refuse_or_forward(&state, "downloadComplianceReport", request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Three permissions across four routes**, and the two that share a resource do not share a
    /// permission.
    #[test]
    fn the_download_permission_is_not_the_read_one() {
        assert_eq!(
            PERMISSION_READ_COMPLIANCE_EXPORT_JOB.id,
            "read_compliance_export_job"
        );
        assert_eq!(
            PERMISSION_CREATE_COMPLIANCE_EXPORT_JOB.id,
            "create_compliance_export_job"
        );
        assert_eq!(
            PERMISSION_DOWNLOAD_COMPLIANCE_EXPORT_RESULT.id,
            "download_compliance_export_result"
        );
        assert_ne!(
            PERMISSION_READ_COMPLIANCE_EXPORT_JOB.id,
            PERMISSION_DOWNLOAD_COMPLIANCE_EXPORT_RESULT.id,
            "reading a report and downloading it are two permissions"
        );
    }

    /// The refusal is a 501 naming the *licence*, unlike the neighbouring compliance-flavoured
    /// data-retention id, which names the feature.
    #[test]
    fn the_refusal_names_the_licence() {
        assert_eq!(
            COMPLIANCE_LICENCE_DISABLED,
            "ent.compliance.licence_disable.app_error"
        );
        assert!(
            COMPLIANCE_LICENCE_DISABLED.starts_with("ent."),
            "the enterprise-interface refusals carry an `ent.` prefix; the api4-level ones do not"
        );
    }
}
