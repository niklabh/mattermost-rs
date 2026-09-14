//! Port of `App.ValidateDesktopToken` (app/desktop_login.go:27), the app half of
//! `POST /api/v4/users/login/desktop_token`.
//!
//! `GenerateAndSaveDesktopToken` (desktop_login.go:13) is not ported: its only callers are the
//! browser-side OAuth and SAML completion pages (web/oauth.go:390, web/saml.go:220), which are
//! not served here. The rows this reads are written by Go.

use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::DesktopTokensStore;

use crate::App;

/// `model.DesktopTokenTTL` (user.go:69) — `time.Minute * 3`, in the seconds the table keeps.
pub const DESKTOP_TOKEN_TTL_SECONDS: i64 = 3 * 60;

impl App {
    /// Port of `App.ValidateDesktopToken` (app/desktop_login.go:27).
    ///
    /// `expiry_time` is the caller's cut-off in Unix **seconds** — `now - DesktopTokenTTL` — and
    /// a row older than it is as absent as one never written: both are the 401
    /// `app.desktop_token.validate.invalid`. A token that resolves to a user the store cannot
    /// load is the 500 `app.desktop_token.validate.no_user`.
    ///
    /// # Every outcome deletes something
    ///
    /// A miss deletes the token (a no-op when it was never there, the clean-up when it was
    /// merely expired); a user-load failure deletes the token too; and a hit deletes **every**
    /// token for the user, this one included — the token is single-use, and any other pending
    /// one for the same account goes with it. A failure of any of those deletes is logged and
    /// does not change the answer, exactly as Go's `a.Log().Error` does.
    #[tracing::instrument(skip_all, fields(expiry_time, user_id))]
    pub async fn validate_desktop_token(&self, token: &str, expiry_time: i64) -> AppResult<User> {
        let tokens = self.store().desktop_tokens();

        let user_id = match tokens.get_user_id(token, expiry_time).await {
            Ok(user_id) => user_id,
            Err(err) => {
                if let Err(delete_err) = tokens.delete(token).await {
                    tracing::error!(error = %delete_err, "Unable to delete desktop token");
                }
                return Err(AppError::boxed(
                    "ValidateDesktopToken",
                    "app.desktop_token.validate.invalid",
                    None,
                    err.to_string(),
                    401,
                ));
            }
        };
        tracing::Span::current().record("user_id", user_id.as_str());

        let user = match self.get_user(&user_id).await {
            Ok(user) => user,
            Err(err) => {
                if let Err(delete_err) = tokens.delete(token).await {
                    tracing::error!(error = %delete_err, "Unable to delete desktop token");
                }
                return Err(AppError::boxed(
                    "ValidateDesktopToken",
                    "app.desktop_token.validate.no_user",
                    None,
                    err.to_string(),
                    500,
                ));
            }
        };

        if let Err(delete_err) = tokens.delete_by_user_id(&user_id).await {
            tracing::error!(error = %delete_err, "Unable to delete desktop token");
        }

        Ok(user)
    }
}
