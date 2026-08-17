//! Port of `getUser` (channels/api4/user.go:305), reached as `GET /api/v4/users/me`.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.HeaderEtagServer`.
const HEADER_ETAG_SERVER: &str = "ETag";

/// Port of `getUser` for the `me` case only.
///
/// # What is reproduced
///
/// The self-branch of Go's handler: fetch the user, compute the etag from the two privacy
/// settings, answer 304 when the client's `If-None-Match` matches, otherwise `Sanitize` for self
/// and write the user with the etag header.
///
/// # What is not, and why it is safe here
///
/// * **The permission check.** Go calls `UserCanSeeOtherUser` for an arbitrary target id. This
///   route only ever resolves `me`, so the target is the session's own user and the check is
///   `true` by construction. Serving another user's id through this handler would need the check
///   first — see D-082 before adding `GET /users/{id}`.
/// * **Terms of service.** Go overwrites `terms_of_service_id` / `_create_at` from the
///   `UserTermsOfService` store when the viewer is the user or an admin. That store is not
///   ported, so both fields stay zero. A server with a ToS policy configured would answer
///   differently here (D-083).
/// * **`UpdateLastActivityAtIfNeeded`.** A write on the read path; deferred with the session
///   cache it belongs to (D-084).
/// * **Privacy settings** come from `ServiceSettings`, which is unported. Go's defaults are used
///   and the etag is computed from them (D-085).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_user_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let mut user = state.app.get_user(&session.0.user_id).await?;

    // `user.Etag(*c.App.Config().PrivacySettings.ShowFullName, *...ShowEmailAddress)`.
    let etag = user.etag(state.show_full_name, state.show_email_address);

    // Port of `Context.HandleEtag`. Go compares the raw header against the computed value; it
    // does not implement weak comparison or a list of candidates.
    if let Some(if_none_match) = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok())
        && if_none_match == etag
    {
        return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
    }

    // `if c.AppContext.Session().UserId == user.Id { user.Sanitize(map[string]bool{}) }`.
    //
    // The empty map is load-bearing and reads backwards. `Sanitize` guards its whole flag block
    // behind `if len(options) != 0` (user.go:702), so an **empty** map strips *nothing* extra —
    // only the four unconditional fields (password, MFA secret, MFA timestamps, last login).
    // Email and full name survive, which is why Go's own `/users/me` returns them.
    //
    // A populated map is the strict one: every flag absent or false strips its field. So the
    // intuitive reading — "no options means no permissions means strip everything" — is exactly
    // inverted, and getting it wrong here would blank the email of every user viewing their own
    // profile. Measured against the running Go server, not inferred.
    user.sanitize(&HashMap::new());

    // Go writes the etag header and then `json.NewEncoder(w).Encode(user)` (user.go:353).
    //
    // `Encode` appends a newline; `json.Marshal` does not. That one byte is the entire difference
    // between this response and Go's — measured, not guessed: the first 720 bytes of both
    // matched exactly, including key order, and only the trailing 0x0a was missing. Any handler
    // ported from an `Encode` call site owes the same newline. See D-086.
    let mut body = serde_json::to_vec(&user).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise User");
        ApiError(mm_model::utils::AppError::new(
            "getUser",
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
            (HEADER_ETAG_SERVER, etag.as_str()),
            ("Content-Type", "application/json"),
            // The operator-facing marker the proxy sets to "go"; this route is the other case.
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use mm_model::user::User;
    use std::collections::HashMap;

    /// The four fields `Sanitize` clears unconditionally. These are the credential-bearing ones,
    /// and they go regardless of any flag.
    #[test]
    fn sanitize_for_self_strips_the_unconditional_secrets() {
        let mut user = User {
            password: "hunter2".to_owned(),
            mfa_secret: "mfa".to_owned(),
            mfa_used_timestamps: Some(vec!["1".to_owned()]),
            last_login: 1_786_973_424_207,
            ..Default::default()
        };

        user.sanitize(&HashMap::new());

        assert!(
            user.password.is_empty(),
            "password must never reach the wire"
        );
        assert!(user.mfa_secret.is_empty());
        assert_eq!(user.mfa_used_timestamps, None);
        assert_eq!(user.last_login, 0);
    }

    /// The inverted-looking half, and the one that would break `/users/me` if read the obvious
    /// way. Go guards the flag block behind `len(options) != 0`, so an EMPTY map keeps email,
    /// full name and auth service — the opposite of "no permissions granted". Asserted here
    /// because the handler passes exactly this empty map on every request.
    #[test]
    fn an_empty_options_map_keeps_the_flag_guarded_fields() {
        let mut user = User {
            email: "slice@example.com".to_owned(),
            first_name: "Slice".to_owned(),
            last_name: "Tester".to_owned(),
            auth_service: "gitlab".to_owned(),
            auth_data: Some("some-auth-data".to_owned()),
            last_password_update: 1_786_973_418_231,
            ..Default::default()
        };

        user.sanitize(&HashMap::new());

        assert_eq!(user.email, "slice@example.com");
        assert_eq!(user.first_name, "Slice");
        assert_eq!(user.last_name, "Tester");
        assert_eq!(user.auth_service, "gitlab");
        assert_eq!(user.auth_data.as_deref(), Some("some-auth-data"));
        assert_eq!(user.last_password_update, 1_786_973_418_231);
    }

    /// A populated map is the strict one: a flag that is absent or false strips its field.
    #[test]
    fn a_populated_options_map_strips_what_it_does_not_allow() {
        let mut user = User {
            email: "slice@example.com".to_owned(),
            first_name: "Slice".to_owned(),
            ..Default::default()
        };

        let mut options = HashMap::new();
        options.insert("fullname".to_owned(), true);
        user.sanitize(&options);

        assert!(
            user.email.is_empty(),
            "email not flagged, so it is stripped"
        );
        assert_eq!(
            user.first_name, "Slice",
            "fullname flagged true, so it stays"
        );
    }

    /// The etag's inputs are the two privacy flags, so the same user rendered under different
    /// settings must not collide in a cache.
    #[test]
    fn the_etag_changes_with_the_privacy_flags() {
        let user = User {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            update_at: 1_786_973_424_207,
            ..Default::default()
        };

        let both = user.etag(true, true);
        assert_ne!(both, user.etag(false, true));
        assert_ne!(both, user.etag(true, false));
        assert_eq!(both, user.etag(true, true), "etag is deterministic");
    }
}
