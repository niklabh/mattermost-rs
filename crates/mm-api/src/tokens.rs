//! The four personal-access-token reads: `getUserAccessTokens` (api4/user.go:3080),
//! `countNonCompliantUserAccessTokens` (:3103), `getUserAccessTokensForUser` (:3155) and
//! `getUserAccessToken` (:3188).
//!
//! # Four routes, four different permission rules
//!
//! Nothing here is shared, and each difference decides who can read someone else's credentials:
//!
//! | route | gate |
//! |---|---|
//! | `GET /users/tokens` | `manage_system` alone — the whole installation's tokens |
//! | `GET /users/tokens/non_compliant/count` | `manage_system` alone |
//! | `GET /users/{user_id}/tokens` | `read_user_access_token`, **then** `SessionHasPermissionToUserOrBot` |
//! | `GET /users/tokens/{token_id}` | `read_user_access_token`, then the same user-or-bot check — **after** the fetch, on the token's owner |
//!
//! The last row is the one a port gets wrong. The id in the URL is the *token's*, so who the
//! caller is being checked against is not known until the row is loaded — which means a caller
//! holding `read_user_access_token` and nothing else learns whether a token id exists (404) before
//! being refused (403). That is Go's ordering and it is reproduced.
//!
//! # The secret is cleared in the app layer, on every one of them
//!
//! The store selects `Token` because `GetByToken` authenticates with it. `mm_app::user_access_token`
//! blanks it, and because the field carries `omitempty` the cleared secret is an **absent key**
//! rather than an empty string.
//!
//! # Three encoders' worth of newline
//!
//! `getUserAccessToken` uses `json.NewEncoder(w).Encode` and ends in a newline; the other three use
//! `json.Marshal` + `w.Write` and do not. Same file, same wire type.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM, PERMISSION_READ_USER_ACCESS_TOKEN,
    make_permission_error,
};
use mm_model::user_access_token::{NonCompliantUserAccessTokenResult, UserAccessToken};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page};
use crate::error::ApiError;

/// The 200 the three `json.Marshal` routes return — **no trailing newline**.
fn json_ok(body: Vec<u8>) -> Response {
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

/// `json.Marshal`, with the id Go uses when it fails.
///
/// **`where` is `searchUserAccessTokens` on two of the three list routes** — a copy-paste in Go
/// (user.go:3094, :3179) that names a handler neither of them is. Not on the wire, and reproduced
/// so the source keeps saying what Go says.
fn encode<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise access tokens");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// Port of `getUserAccessTokens` (user.go:3080) — `GET /api/v4/users/tokens`.
///
/// Every token on the installation, gated on `manage_system` alone. The store's query has **no
/// `ORDER BY`**, so the page order is Postgres's and is not a parity property; see
/// [`mm_store::UserAccessTokenStore`].
#[tracing::instrument(skip_all, fields(page, per_page, count))]
pub async fn get_user_access_tokens(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let tokens = state.app.get_user_access_tokens(page, per_page).await?;
    tracing::Span::current().record("count", tokens.len());

    Ok(json_ok(encode(&tokens, "searchUserAccessTokens")?))
}

/// Port of `countNonCompliantUserAccessTokens` (user.go:3103) —
/// `GET /api/v4/users/tokens/non_compliant/count`.
///
/// On a stock server this reads **nothing**: the lifetime policy is off, so the app layer returns
/// zero before the query. The body is `{"count":0}` — one key, and `Count` has no `omitempty`, so
/// zero is present rather than omitted.
#[tracing::instrument(skip_all, fields(count))]
pub async fn count_non_compliant_user_access_tokens(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let count = state.app.count_non_compliant_user_access_tokens().await?;
    tracing::Span::current().record("count", count);

    let result = NonCompliantUserAccessTokenResult { count };
    Ok(json_ok(encode(
        &result,
        "countNonCompliantUserAccessTokens",
    )?))
}

/// Port of `getUserAccessTokensForUser` (user.go:3155) — `GET /api/v4/users/{user_id}/tokens`.
///
/// # Two gates, and the second one reports a permission it never checked
///
/// `read_user_access_token` first, then `SessionHasPermissionToUserOrBot` — whose refusal is
/// `SetPermissionError(PermissionEditOtherUsers)`, a permission that check does not itself consult
/// in the branch that usually denies. Reproduced: the error names `edit_other_users`.
///
/// **`me` is not resolved here.** `RequireUserId` does resolve it (web/context.go:301), so
/// `/users/me/tokens` works — the resolution happens before validation, as everywhere else.
#[tracing::instrument(skip_all, fields(user_id = %user_id, page, per_page, count))]
pub async fn get_user_access_tokens_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = crate::channels::resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_USER_ACCESS_TOKEN)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_USER_ACCESS_TOKEN],
        )));
    }

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let tokens = state
        .app
        .get_user_access_tokens_for_user(&user_id, page, per_page)
        .await?;
    tracing::Span::current().record("count", tokens.len());

    Ok(json_ok(encode(&tokens, "searchUserAccessTokens")?))
}

/// Port of `getUserAccessToken` (user.go:3188) — `GET /api/v4/users/tokens/{token_id}`.
///
/// **The fetch sits between the two permission checks.** `read_user_access_token` gates the route,
/// the token is loaded, and only then is the caller checked against the token's *owner*. A caller
/// with the first permission and not the second therefore gets a **404** for an id that does not
/// exist and a **403** for one that does — an existence oracle over token ids, and Go's.
///
/// This is the one route of the four whose body ends in a newline.
#[tracing::instrument(skip_all, fields(token_id = %token_id))]
pub async fn get_user_access_token(
    State(state): State<AppState>,
    Path(token_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireTokenId` (web/context.go) — the URL-param error.
    if !is_valid_id(&token_id) {
        return Err(ApiError::invalid_url_param("token_id"));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_USER_ACCESS_TOKEN)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_USER_ACCESS_TOKEN],
        )));
    }

    // `sanitize: true` — the handler's own argument, and the reason the secret never reaches a
    // client through this route while the revocation paths still see it.
    let token: UserAccessToken = state.app.get_user_access_token(&token_id, true).await?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &token.user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let mut body = encode(&token, "getUserAccessToken")?;
    body.push(b'\n');
    Ok(json_ok(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sanitised token serialises **without a `token` key at all** — `omitempty` on an empty
    /// string. A port that modelled the field as `Option<String>` and left it `Some("")` would
    /// emit `"token":""`, which is a different document and a hint that a secret exists.
    #[test]
    fn a_sanitised_token_has_no_token_key() {
        let token = UserAccessToken {
            id: "j1x3z8ynqjbstd4c4k6qy1p7ph".to_owned(),
            token: String::new(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            description: "personal".to_owned(),
            is_active: true,
            expires_at: 0,
            last_notified_at: Some(17),
        };

        let wire: serde_json::Value =
            serde_json::from_slice(&encode(&token, "getUserAccessToken").expect("serialises"))
                .expect("json");
        let object = wire.as_object().expect("an object");

        assert!(!object.contains_key("token"), "no secret key: {wire}");
        // `LastNotifiedAt` is `json:"-"`, so it is absent whatever it holds.
        assert!(!object.contains_key("last_notified_at"), "{wire}");
        assert_eq!(object.len(), 5, "five keys survive: {wire}");
        assert_eq!(wire["expires_at"], 0, "zero is present, not omitted");
    }

    /// `{"count":0}` — `Count` carries no `omitempty`, so the zero this route always answers on a
    /// stock server is on the wire rather than an empty object.
    #[test]
    fn the_count_result_keeps_a_zero() {
        let body = encode(
            &NonCompliantUserAccessTokenResult { count: 0 },
            "countNonCompliantUserAccessTokens",
        )
        .expect("serialises");
        assert_eq!(String::from_utf8_lossy(&body), r#"{"count":0}"#);
    }
}
