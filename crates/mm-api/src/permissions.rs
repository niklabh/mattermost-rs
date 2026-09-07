//! Port of `appendAncillaryPermissionsPost` (channels/api4/permission.go:18), reached as
//! `POST /api/v4/permissions/ancillary`.
//!
//! The system console's role editor posts the sysconsole permissions an admin ticked and gets
//! back that list plus the ancillary permissions each one implies. It is the only route under
//! `/permissions`, it touches no database, and its whole behaviour is
//! [`mm_model::role::add_ancillary_permissions`].

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::role::add_ancillary_permissions;
use mm_model::utils::{AppError, remove_duplicate_strings_non_sort};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.PayloadParseError` (model/utils.go:42).
const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";

/// Port of `appendAncillaryPermissionsPost` (permission.go:18).
///
/// # No permission check
///
/// `APISessionRequired` and nothing else. Any authenticated session may expand a permission list;
/// the route tells you what a role *would* imply, not what you hold.
///
/// # `len(permissions) < 1` and a decode failure are the **same** 400
///
/// `model.NonSortedArrayFromJSON` returns `(nil, nil)` for a JSON `null` — the error is `nil` and
/// the slice is not — and the handler's condition is `err != nil || len(permissions) < 1`. So
/// `null`, `[]` and a malformed body all produce `api.payload.parse.error` at 400, with the same
/// empty `detailed_error`. Three inputs, one answer.
///
/// # The body is deduplicated on the way *in*, and not on the way out
///
/// `NonSortedArrayFromJSON` calls `RemoveDuplicateStringsNonSort` (utils.go:565), so a request
/// listing the same permission twice is collapsed **before** expansion. Nothing dedups the
/// result, so the ancillary permissions of two different inputs may repeat in the response — and
/// they do: several sysconsole reads imply the same underlying permission. Reproduced, because
/// the console counts what it gets back.
///
/// # The expansion is one pass, not a closure
///
/// Go ranges over the slice it is appending to, and `range` fixes the length at loop entry — so
/// an ancillary permission that itself has ancillary permissions is **not** expanded. See the
/// note on [`add_ancillary_permissions`], which carries that subtlety.
///
/// `json.Marshal` and `w.Write`: **no** trailing newline.
#[tracing::instrument(skip_all, fields(asked, returned))]
pub async fn append_ancillary_permissions_post(
    State(_state): State<AppState>,
    _session: AuthenticatedSession,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let permissions = parse_permissions(&body).ok_or_else(|| {
        ApiError::from(AppError::new(
            "appendAncillaryPermissionsPost",
            PAYLOAD_PARSE_ERROR,
            None,
            String::new(),
            400,
        ))
    })?;
    tracing::Span::current().record("asked", permissions.len());

    let expanded = add_ancillary_permissions(permissions);
    tracing::Span::current().record("returned", expanded.len());

    let out = serde_json::to_vec(&expanded).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the permission list");
        // `c.SetJSONEncodingError` (web/context.go) — `api.marshal_error`, 500.
        ApiError::from(AppError::new(
            "appendAncillaryPermissionsPost",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        out,
    )
        .into_response())
}

/// `model.NonSortedArrayFromJSON` plus the handler's `len < 1` guard, as one decision.
///
/// [`None`] is "answer 400", which is what Go's `err != nil || len(permissions) < 1` collapses to.
/// Separating them would invite a caller to distinguish two cases the wire cannot.
fn parse_permissions(body: &[u8]) -> Option<Vec<String>> {
    let parsed: Vec<String> = serde_json::from_slice(body).ok()?;
    let deduped = remove_duplicate_strings_non_sort(&parsed);
    (!deduped.is_empty()).then_some(deduped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three bodies that are one 400.
    #[test]
    fn null_empty_and_malformed_are_all_refused() {
        assert_eq!(parse_permissions(b"null"), None, "Go's nil slice");
        assert_eq!(parse_permissions(b"[]"), None, "`len < 1`");
        assert_eq!(parse_permissions(b"{"), None, "a decode failure");
        assert_eq!(
            parse_permissions(b"{\"a\":1}"),
            None,
            "an object is not an array of strings"
        );
        assert_eq!(parse_permissions(b""), None, "an empty body");
    }

    /// Duplicates are collapsed **before** expansion, in the order they arrived.
    #[test]
    fn the_input_is_deduplicated_and_not_sorted() {
        assert_eq!(
            parse_permissions(br#"["b","a","b","c","a"]"#),
            Some(vec!["b".to_owned(), "a".to_owned(), "c".to_owned()]),
            "first occurrence wins and the order is the caller's"
        );
    }

    /// A permission with no ancillary entry comes back unchanged and alone — so the route is not
    /// silently adding anything of its own.
    #[test]
    fn an_unknown_permission_is_echoed_untouched() {
        let out = add_ancillary_permissions(vec!["not_a_real_permission".to_owned()]);
        assert_eq!(out, vec!["not_a_real_permission".to_owned()]);
    }

    /// A real sysconsole permission gains its ancillaries, appended after the input.
    ///
    /// Asserted as "grew, and kept the input first" rather than against a literal list: the table
    /// is generated from the Go source and pinning its contents here would duplicate
    /// `mm_model::role`'s own generated-table test, which is where a change to it belongs.
    #[test]
    fn a_sysconsole_permission_gains_its_ancillaries() {
        let key = "sysconsole_write_user_management_channels";
        let out = add_ancillary_permissions(vec![key.to_owned()]);
        assert_eq!(out.first().map(String::as_str), Some(key));
        assert!(
            out.len() > 1,
            "the table has ancillary permissions for {key}: {out:?}"
        );
    }
}
