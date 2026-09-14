//! Port of `updateViewedProductNotices` (api4/system.go) — `PUT /api/v4/system/notices/view`,
//! the webapp marking the in-product notices a user has dismissed.
//!
//! # The body is `SortedArrayFromJSON`, and `null` is not an error
//!
//! `json.NewDecoder(r.Body).Decode(&[]string)`: a decode failure — bad JSON, an object, a number
//! in the list — is the 400 `api.payload.parse.error`; a `null` decodes to a nil slice with no
//! error and is handed on as an empty list, so it answers `{"status":"OK"}` like `[]`. What is
//! decoded is **sorted and de-duplicated** (`RemoveDuplicateStrings` sorts in place), so a list
//! naming one notice twice counts one viewing.
//!
//! Then `UpdateViewedProductNotices`, whose only failure is the store's, at 400 with
//! `api.system.update_viewed_notices.failed` — and `ReturnStatusOK` with no trailing newline.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;
use mm_store::ProductNoticesStore;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `model.SortedArrayFromJSON` (utils.go:546): `Err` for a body that does not decode as a
/// list of strings, `Ok(empty)` for `null`, otherwise the sorted, de-duplicated ids.
fn sorted_array_from_json(bytes: &[u8]) -> Result<Vec<String>, serde_json::Error> {
    let mut ids: Vec<String> = match serde_json::from_slice::<Option<Vec<String>>>(bytes)? {
        Some(ids) => ids,
        None => return Ok(Vec::new()),
    };
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Port of `updateViewedProductNotices` — `PUT /api/v4/system/notices/view`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, notices))]
pub async fn update_viewed_product_notices(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let ids = match sorted_array_from_json(&bytes) {
        Ok(ids) => ids,
        Err(err) => {
            return ApiError::from(AppError::new(
                "updateViewedProductNotices",
                "api.payload.parse.error",
                None,
                err.to_string(),
                400,
            ))
            .into_response();
        }
    };
    tracing::Span::current().record("notices", ids.len());

    if let Err(err) = state
        .app
        .store()
        .product_notices()
        .view(&session.0.user_id, &ids)
        .await
    {
        tracing::error!(error = %err, "product notice view write failed");
        return ApiError::from(AppError::new(
            "UpdateViewedProductNotices",
            "api.system.update_viewed_notices.failed",
            None,
            err.to_string(),
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

#[cfg(test)]
mod tests {
    use super::sorted_array_from_json;

    /// `null` is a nil slice and no error; the rest of the non-lists are errors; a list is
    /// sorted and de-duplicated.
    #[test]
    fn the_body_decodes_like_sorted_array_from_json() {
        assert_eq!(
            sorted_array_from_json(b"null").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(sorted_array_from_json(b"[]").unwrap(), Vec::<String>::new());
        assert_eq!(
            sorted_array_from_json(br#"["b","a","b"]"#).unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        for bad in [&b"{}"[..], b"[1]", b"\"a\"", b"", b"[\"a\","] {
            assert!(
                sorted_array_from_json(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
