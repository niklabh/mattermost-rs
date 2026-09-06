//! Port of `getLatestTermsOfService` (channels/api4/terms_of_service.go:20), reached as
//! `GET /api/v4/terms_of_service`.
//!
//! The webapp asks for this at login when custom terms of service are switched on, and shows the
//! text before letting anyone in.
//!
//! # A session, and nothing else
//!
//! `APISessionRequired`, and then **no permission check at all** — the terms are what a user must
//! read before they can use the server, so gating them behind a permission would be circular. Two
//! lines of handler: fetch, encode.
//!
//! # Publishing is licence-gated; reading is not
//!
//! `createTermsOfService` beside it refuses without `CustomTermsOfService`
//! (terms_of_service.go:38), so on this deployment the table can only be written by hand. Reading
//! is ungated, which is why this route is portable and its neighbour is not.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `getLatestTermsOfService` (terms_of_service.go:20).
///
/// An empty table is a **404** with `app.terms_of_service.get.no_rows.app_error` — not an empty
/// object and not a `null` — which is what a client checks to decide there is nothing to show.
///
/// `json.NewEncoder(w).Encode`, so a trailing newline.
#[tracing::instrument(skip_all, fields(id))]
pub async fn get_latest_terms_of_service(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let terms = state.app.get_latest_terms_of_service().await?;
    tracing::Span::current().record("id", &terms.id);

    let mut body = serde_json::to_vec(&terms).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the terms of service");
        ApiError::from(AppError::new(
            "getLatestTermsOfService",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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
