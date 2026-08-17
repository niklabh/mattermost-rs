//! Port of `getSessions` (channels/api4/user.go:2570), reached as
//! `GET /api/v4/users/me/sessions`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `getSessions` for the `me` case only.
///
/// # The two things that matter
///
/// **`Sanitize` is not optional.** Every session in this list carries the bearer token that
/// authenticates it. Go calls `session.Sanitize()` on each one (user.go:2588), which clears
/// `Token` and nothing else. Skipping it would hand a caller every one of their own live
/// credentials in plaintext — and, worse, would do so through an endpoint whose whole purpose is
/// to be shown in a UI. There is a test below that fails if the call is removed.
///
/// **This handler uses `json.Marshal`, not `json.NewEncoder().Encode()`** (user.go:2592), so —
/// unlike `/users/me` — the body carries **no trailing newline**. Same wire type, same server,
/// different call site, different bytes. See [D-086]; this is the second instance and the first
/// where the answer goes the other way.
///
/// # Not reproduced
///
/// Go checks `SessionHasPermissionToUser(session, userId)` before anything else. This route
/// resolves `me` only, so the target is the session's own user and the check is `true` by
/// construction — the same reasoning, and the same caveat, as [D-082]. Serving another user's id
/// through this handler needs the check first.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_sessions_me(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let mut sessions = state.app.get_sessions(&session.0.user_id).await?;

    // `for _, session := range sessions { session.Sanitize() }` — clears the token, leaving
    // everything else, including `props`, which may still hold the CSRF value.
    for session in &mut sessions {
        session.sanitize();
    }

    tracing::Span::current().record("count", sessions.len());

    // `json.Marshal` then `w.Write` — no newline appended. Deliberate; see the note above.
    let body = serde_json::to_vec(&sessions).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise sessions");
        ApiError(mm_model::utils::AppError::new(
            "getSessions",
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
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use mm_model::session::Session;

    /// The security property of this endpoint, asserted directly. If `sanitize` stops clearing
    /// the token — or the handler stops calling it — this is what should fail.
    #[test]
    fn sanitize_clears_the_token_and_nothing_else() {
        let mut session = Session {
            id: "sessionid".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            roles: "system_user".to_owned(),
            device_id: "device".to_owned(),
            ..Default::default()
        };

        session.sanitize();

        assert_eq!(session.token, "", "the token must never reach a client");
        // Everything else survives — Sanitize is one line in Go and widening it here would
        // silently drop fields the webapp reads.
        assert_eq!(session.id, "sessionid");
        assert_eq!(session.user_id, "y9i4er48tt8bukijy7i3u5y9ar");
        assert_eq!(session.roles, "system_user");
        assert_eq!(session.device_id, "device");
    }

    /// A sanitised list must contain no token anywhere in its serialised form — the check a
    /// reviewer would actually want, rather than a per-field assertion that can miss one.
    #[test]
    fn no_token_survives_serialisation_of_the_list() {
        let secret = "cqjc7ec6bpy65jjamstkhpe6fr";
        let mut sessions = vec![
            Session {
                id: "one".to_owned(),
                token: secret.to_owned(),
                ..Default::default()
            },
            Session {
                id: "two".to_owned(),
                token: secret.to_owned(),
                ..Default::default()
            },
        ];

        for session in &mut sessions {
            session.sanitize();
        }

        let json = serde_json::to_string(&sessions).expect("serialises");
        assert!(
            !json.contains(secret),
            "a token survived into the response body: {json}"
        );
        assert!(json.contains("\"token\":\"\""), "the key stays, empty");
    }

    /// Unlike `/users/me`, this handler must NOT append a newline — Go uses `json.Marshal` here
    /// rather than an encoder. Pinning it so the two call sites cannot be conflated later.
    #[test]
    fn the_body_has_no_trailing_newline() {
        let sessions: Vec<Session> = vec![Session::default()];
        let body = serde_json::to_vec(&sessions).expect("serialises");
        assert_ne!(
            body.last(),
            Some(&b'\n'),
            "json.Marshal appends nothing; only Encode does"
        );
    }

    /// An empty list is `[]`, not `null` — a user whose sessions were all revoked still gets a
    /// well-formed array.
    #[test]
    fn an_empty_list_serialises_as_an_array() {
        let sessions: Vec<Session> = Vec::new();
        assert_eq!(serde_json::to_string(&sessions).expect("serialises"), "[]");
    }
}
