//! Port of `web.Context.LogAudit` and `LogAuditWithUserId` (web/context.go:95, :103) for the
//! handlers this server answers.
//!
//! Go reads four things off the request context when it writes an `Audits` row: the session's
//! user and id, `c.AppContext.IPAddress()` and `c.AppContext.Path()`. The session is the
//! handler's own ([`crate::auth::AuthenticatedSession`], or none on the local socket and on the
//! pre-login routes); the other two are the request's, captured here as an [`AuditRequest`]
//! extractor so a handler that has consumed its request can still write the row.
//!
//! Where a handler calls these — on entry, on a refusal, on success — is Go's and is repeated at
//! the call site; see [D-270] for the table of every call.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mm_app::App;
use mm_model::session::Session;

/// `c.AppContext.Path()` and `c.AppContext.IPAddress()` for one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditRequest {
    /// `r.URL.Path` — no query string.
    pub path: String,
    /// `utils.GetIPAddress` over `TrustedProxyIPHeader`; see [`crate::client_ip`].
    pub ip_address: String,
}

impl AuditRequest {
    /// The two fields, read off request parts.
    pub fn of(parts: &Parts) -> Self {
        Self {
            path: parts.uri.path().to_owned(),
            ip_address: crate::client_ip::client_ip(&parts.headers, &parts.extensions),
        }
    }

    /// [`AuditRequest::of`] for a handler that still holds the whole request.
    pub fn of_request<B>(request: &axum::http::Request<B>) -> Self {
        Self {
            path: request.uri().path().to_owned(),
            ip_address: crate::client_ip::client_ip(request.headers(), request.extensions()),
        }
    }

    /// Port of `c.LogAudit(extraInfo)`: the session's user and id, or two empty strings when
    /// there is no session (the local socket, a request before login).
    pub async fn log(&self, app: &App, session: Option<&Session>, extra_info: &str) {
        let (user_id, session_id) = session_ids(session);
        app.log_audit(
            user_id,
            session_id,
            &self.ip_address,
            &self.path,
            extra_info,
        )
        .await;
    }

    /// Port of `c.LogAuditWithUserId(userId, extraInfo)`: the row is attributed to `user_id`,
    /// and when the session has a user, ` session_user=<id>` is appended and the whole text
    /// trimmed — so an empty `extra_info` on a session becomes `session_user=<id>` with no
    /// leading space.
    pub async fn log_with_user_id(
        &self,
        app: &App,
        session: Option<&Session>,
        user_id: &str,
        extra_info: &str,
    ) {
        let (session_user, session_id) = session_ids(session);
        let extra_info = with_session_user(extra_info, session_user);
        app.log_audit(
            user_id,
            session_id,
            &self.ip_address,
            &self.path,
            &extra_info,
        )
        .await;
    }
}

fn session_ids(session: Option<&Session>) -> (&str, &str) {
    session.map_or(("", ""), |s| (s.user_id.as_str(), s.id.as_str()))
}

/// `strings.TrimSpace(extraInfo + " session_user=" + userId)`, only when there is a session user.
fn with_session_user(extra_info: &str, session_user: &str) -> String {
    if session_user.is_empty() {
        extra_info.to_owned()
    } else {
        format!("{extra_info} session_user={session_user}")
            .trim()
            .to_owned()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AuditRequest {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::of(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go appends the suffix only when the session has a user, and `TrimSpace`s the result — so
    /// an empty text gains no leading space and a text with trailing space loses it.
    #[test]
    fn the_session_user_suffix_is_appended_only_for_a_session_user_and_trimmed() {
        assert_eq!(with_session_user("success", ""), "success");
        assert_eq!(with_session_user("", ""), "");
        assert_eq!(
            with_session_user("success", "abc"),
            "success session_user=abc"
        );
        assert_eq!(with_session_user("", "abc"), "session_user=abc");
        assert_eq!(
            with_session_user("  padded ", "abc"),
            "padded  session_user=abc"
        );
    }

    #[test]
    fn the_path_carries_no_query_string() {
        let request = axum::http::Request::builder()
            .uri("/api/v4/users/login?x=1")
            .body(())
            .unwrap();
        let audit = AuditRequest::of_request(&request);
        assert_eq!(audit.path, "/api/v4/users/login");
    }
}
