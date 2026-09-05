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
//! # Only the unlicensed answer is ours
//!
//! `c.App.Srv().ClientLicense()` reads a map Go built **at startup** from the licence body. That
//! body is signed, its client map is derived from the enterprise feature set, and neither is
//! ported — so a licensed installation is forwarded and Go answers it. See
//! [`mm_app::license::LicenseState`] for how a second process can tell the difference.
//!
//! On the unlicensed path the two branches of the permission check **converge**: Go's fallback map
//! is `{"IsLicensed": "false"}`, and `GetSanitizedClientLicense` only ever deletes keys
//! (`utils.GetSanitizedClientLicense`, utils/license.go:273) — `Id`, `Name`, `Email`, `IssuedAt`,
//! `StartsAt`, `ExpiresAt`, `SkuName`, none of which is present. So an admin and an anonymous
//! caller get identical bytes, and `read_license_information` is not consulted here at all. That
//! is not a shortcut: a permission check whose two outcomes are indistinguishable cannot be
//! tested, and writing one would be an untested claim about a branch this server never reaches.
//! When a licensed installation stops being forwarded, the check lands with the map it gates.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::license::LicenseState;
use mm_model::utils::AppError;

use crate::AppState;
use crate::channels::query_first;
use crate::error::ApiError;
use crate::proxy;

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
#[tracing::instrument(skip_all, fields(format, licensed))]
pub async fn get_client_license(State(state): State<AppState>, request: Request) -> Response {
    let format = query_first(request.uri().query(), FORMAT_PARAM).unwrap_or_default();
    tracing::Span::current().record("format", &format);

    if format.is_empty() {
        return old_format_error().into_response();
    }
    if format != FORMAT_OLD {
        return ApiError::invalid_param(FORMAT_PARAM).into_response();
    }

    let state_of_licence = match state.app.license_state().await {
        Ok(state_of_licence) => state_of_licence,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("licensed", state_of_licence == LicenseState::Licensed);

    if state_of_licence == LicenseState::Licensed {
        return proxy::forward_to_go(State(state), request).await;
    }

    let map = LicenseState::unlicensed_client_license();
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
