//! Port of `app.GetSession` (channels/app/session.go:86).

use std::collections::HashMap;

use mm_model::session::Session;
use mm_model::utils::AppError;
use mm_store::SessionStore;

use crate::App;

/// Go's error id for every failure on this path, whatever the cause. 401 in all cases.
const INVALID_TOKEN: &str = "api.context.invalid_token.error";

impl App {
    /// Port of `app.App.GetSession` (session.go:86).
    ///
    /// # The token-vs-id check is load-bearing
    ///
    /// `SessionStore.Get` matches `Token = $1 OR Id = $1`, so it will happily return a session
    /// when the caller passed a session **id**. Go catches that one line later — `if session.Token
    /// != token` — and rejects it (session.go:95). Dropping that check would turn session ids into
    /// bearer credentials, and session ids are far less protected than tokens: they appear in
    /// admin APIs and logs. It is reproduced here for that reason, not for tidiness.
    ///
    /// # What this port does not do
    ///
    /// Go additionally consults a session cache, mints a session from a *user access token* when
    /// the lookup misses, and revokes sessions past `SessionIdleTimeoutInMinutes`. None of that is
    /// ported: the cache is an optimisation, and the other two need config that does not exist
    /// here yet. Each is recorded in `docs/TECH_DEBT.md`; the idle timeout is the one with a
    /// behavioural consequence, since an idle session this accepts is one Go would revoke.
    #[tracing::instrument(skip_all, fields(session_id, user_id))]
    pub async fn get_session(&self, token: &str) -> Result<Session, AppError> {
        let session = match self.store().session().get(token).await {
            Ok(session) => session,
            Err(err) => {
                // Go skips the error check entirely here and only tests whether a session came
                // back, because a miss is a legitimate route into the access-token path. The
                // distinction still matters for us: a broken query must not be reported to the
                // client as a bad token.
                if !err.is_not_found() {
                    tracing::error!(error = %err, "session lookup failed");
                    return Err(AppError::new(
                        "GetSession",
                        "app.session.get.app_error",
                        None,
                        String::new(),
                        500,
                    ));
                }
                return Err(invalid_token("session not found"));
            }
        };

        if session.token != token {
            return Err(invalid_token(
                "session token is different from the one in DB",
            ));
        }

        if session.id.is_empty() || session.is_expired() {
            return Err(invalid_token("session is either nil or expired"));
        }

        tracing::Span::current().record("session_id", &session.id);
        tracing::Span::current().record("user_id", &session.user_id);
        Ok(session)
    }
}

/// Go passes `map[string]any{"Token": token, "Error": ""}` as the params. The token is a live
/// credential and `AppError`'s params are not serialised (`json:"-"`), but they do reach the i18n
/// layer and any logger that formats the struct — so the token is omitted rather than carried.
/// See D-079.
fn invalid_token(details: &str) -> AppError {
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();
    params.insert("Error".to_owned(), serde_json::Value::String(String::new()));
    AppError::new("GetSession", INVALID_TOKEN, Some(params), details, 401)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_token_is_401_with_gos_error_id() {
        let err = invalid_token("session is either nil or expired");
        assert_eq!(err.id, INVALID_TOKEN);
        assert_eq!(err.status_code, 401);
        assert_eq!(err.where_, "GetSession");
        assert_eq!(err.detailed_error, "session is either nil or expired");
    }

    /// The params map reaches loggers. Go puts the token in it; we must not.
    #[test]
    fn invalid_token_params_omit_the_token() {
        let err = invalid_token("whatever");
        let params = err.params.expect("params are set");
        assert!(!params.contains_key("Token"));
        assert_eq!(
            params.get("Error"),
            Some(&serde_json::Value::String(String::new()))
        );
    }

    /// `AppError::new` starts `message` at `id`, which is what an untranslated Go error renders
    /// as. The client sees this string, so it is part of the wire format.
    #[test]
    fn message_defaults_to_the_error_id() {
        let err = invalid_token("x");
        assert_eq!(err.message, INVALID_TOKEN);
    }
}
