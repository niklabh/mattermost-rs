//! Port of `getClientLicense` (channels/api4/license.go:31), reached as
//! `GET /api/v4/license/client?format=old`.
//!
//! Every webapp load calls this before it renders anything, alongside `/config/client`, so it is
//! one of the two or three hottest routes on the server and one of the smallest.
//!
//! # `APIHandler`, not `APISessionRequired`
//!
//! It is registered with `api.APIHandler` (license.go:25), so **no session is required** and an
//! anonymous request is answered rather than refused. The session still matters — it decides
//! between the full and the sanitized map — but its absence is not an error, which is why nothing
//! here takes [`crate::auth::AuthenticatedSession`].
//!
//! # Both maps are ours
//!
//! `c.App.Srv().ClientLicense()` reads a map Go built **at startup** from the licence body
//! (`utils.GetClientLicense`); [`mm_app::App::client_license`] builds the same map from the same
//! body, verified the same way, on demand. Until 2026-09-13 only the unlicensed fallback was
//! served and a licensed installation was forwarded — the licence could be detected but not read.
//!
//! # The permission decides the map, and only on a licensed server
//!
//! `read_license_information` selects the full map; everyone else — a plain user, and an
//! anonymous caller, since `APIHandler` requires no session — gets `GetSanitizedClientLicense`,
//! which deletes seven keys (`Id`, `Name`, `Email`, `IssuedAt`, `StartsAt`, `ExpiresAt`,
//! `SkuName`; utils/license.go:273). On an unlicensed server the two branches **converge**: the
//! fallback map is `{"IsLicensed": "false"}` and sanitising only deletes, so an admin and an
//! anonymous caller get identical bytes. The check is made regardless, because the answer is the
//! same and the code path is then the one a licensed server takes.
//!
//! An anonymous caller has no session, and `SessionHasPermissionTo` on the zero-valued session
//! Go carries for one is false: no roles, not unrestricted. [`OptionalSession`] reproduces the
//! zero session as `None`, and `None` is the sanitized branch.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::PERMISSION_READ_LICENSE_INFORMATION;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth_writes::OptionalSession;
use crate::channels::query_first;
use crate::error::ApiError;

/// The query parameter, and the only value Go accepts for it.
const FORMAT_PARAM: &str = "format";
const FORMAT_OLD: &str = "old";

/// Port of `getClientLicense` (license.go:31).
///
/// # Two different 400s, and the order they are checked in
///
/// An absent **or empty** `format` is `api.license.client.old_format.app_error`; any other
/// non-`old` value is `SetInvalidParam("format")` — `api.context.invalid_body_param.app_error`,
/// whose id says *body* param even though this is a query string, because `SetInvalidParam` is
/// the body-param constructor and the handler reaches for it anyway. The comparison is
/// case-sensitive: `format=OLD` is the second error, not a success.
///
/// The empty case is checked first, so `?format=` cannot reach the second branch. Getting that
/// order backwards changes the id a client sees for the commonest mistake — omitting the
/// parameter — which is exactly the sort of thing the webapp branches on.
#[tracing::instrument(skip_all, fields(format, sanitized))]
pub async fn get_client_license(
    State(state): State<AppState>,
    session: OptionalSession,
    request: Request,
) -> Response {
    let format = query_first(request.uri().query(), FORMAT_PARAM).unwrap_or_default();
    tracing::Span::current().record("format", &format);

    if format.is_empty() {
        return old_format_error().into_response();
    }
    if format != FORMAT_OLD {
        return ApiError::invalid_param(FORMAT_PARAM).into_response();
    }

    let may_read_everything = match &session.0 {
        Some(session) => {
            state
                .app
                .session_has_permission_to(session, &PERMISSION_READ_LICENSE_INFORMATION)
                .await
        }
        None => false,
    };
    tracing::Span::current().record("sanitized", !may_read_everything);

    let map = if may_read_everything {
        state.app.client_license().await
    } else {
        state.app.sanitized_client_license().await
    };
    let map = match map {
        Ok(map) => map,
        Err(err) => return ApiError::from(err).into_response(),
    };
    match serde_json::to_vec(&map) {
        // `model.MapToJSON` + `w.Write` (license.go:51) — no encoder, so **no trailing newline**.
        Ok(body) => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the client licence");
            ApiError::from(AppError::new(
                "getClientLicense",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// `model.NewAppError("getClientLicense", "api.license.client.old_format.app_error", nil, "", 400)`
/// (license.go:34).
fn old_format_error() -> ApiError {
    ApiError::from(AppError::new(
        "getClientLicense",
        "api.license.client.old_format.app_error",
        None,
        String::new(),
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two error branches and their order, without a server: `?format=` must not be read as
    /// "some other format".
    #[test]
    fn an_empty_format_is_the_old_format_error_and_not_an_invalid_param() {
        let err = old_format_error();
        assert_eq!(err.0.id, "api.license.client.old_format.app_error");
        assert_eq!(err.0.status_code, 400);

        let other = ApiError::invalid_param(FORMAT_PARAM);
        assert_eq!(other.0.id, "api.context.invalid_body_param.app_error");
        assert_eq!(other.0.status_code, 400);
        assert_ne!(err.0.id, other.0.id, "the two 400s are not interchangeable");
    }

    /// `url.Values.Get` semantics on the one parameter this route reads.
    #[test]
    fn the_format_parameter_takes_the_first_value_and_decodes_it() {
        assert_eq!(
            query_first(Some("format=old"), FORMAT_PARAM).as_deref(),
            Some("old")
        );
        assert_eq!(
            query_first(Some("format=old&format=new"), FORMAT_PARAM).as_deref(),
            Some("old"),
            "a repeated key takes the first value, as `url.Values.Get` does"
        );
        assert_eq!(
            query_first(Some("format="), FORMAT_PARAM).as_deref(),
            Some("")
        );
        assert_eq!(query_first(Some("other=old"), FORMAT_PARAM), None);
        assert_eq!(
            query_first(Some("format=%6fld"), FORMAT_PARAM).as_deref(),
            Some("old"),
            "percent-escapes are decoded before the comparison, as `url.ParseQuery` does"
        );
    }
}
