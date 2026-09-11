//! The password-write half of `channels/app/authentication.go` and `channels/app/user.go`, plus
//! the session revocation `channels/app/session.go` puts behind them.
//!
//! # Failure modes here are deliberately uniform, and that is the wire format
//!
//! Every branch below answers with an id Go already chose, and several of them are the *same* id
//! for causes a caller might expect to differ — an unknown account and a wrong password both
//! reach `api.user.check_user_password.invalid.app_error`, and a token that never existed and one
//! of the wrong type both reach `api.user.reset_password.invalid_link.app_error`. Splitting any
//! of them into something more informative would turn a migrated route into an account
//! enumeration oracle that the Go server beside it is not. When a branch here looks
//! under-specific, that is the reason.
//!
//! # The counter is claimed before the password is checked
//!
//! `DoubleCheckPassword` increments `Users.FailedAttempts` *first*, conditionally on it being
//! below the cap, and refunds the claim when the failure turns out not to be a credential
//! mismatch. The order is observable: a caller at the cap is refused before their password is
//! looked at, so a correct password does not unlock an already-locked account. See
//! [`App::double_check_password`].
//!
//! # What is not here
//!
//! Login, MFA, LDAP, SAML and the e-mails. `App.UpdatePasswordSendEmail` and
//! `App.SendPasswordReset` both end in `EmailService`, which this tree does not have; the routes
//! that exist *only* to send an e-mail are forwarded to Go rather than answered, and the ones
//! that send one as a side effect of a write do the write and log. See [D-235].

use mm_model::session::Session;
use mm_model::token::{TOKEN_TYPE_PASSWORD_RECOVERY, TOKEN_TYPE_VERIFY_EMAIL, Token};
use mm_model::user::User;
use mm_model::utils::{
    AppError, AppResult, LOWERCASE_LETTERS, NUMBERS, SYMBOLS, UPPERCASE_LETTERS, get_millis,
};
use mm_store::{SessionStore, TokenStore, UserStore};

use crate::App;
use crate::password;

/// `model.PasswordMaximumLength` (config.go:63). The same 72 as the hashers' byte cap, and
/// counted the same way — `len(password)`, bytes.
pub const PASSWORD_MAXIMUM_LENGTH: usize = 72;

/// The id every credential mismatch shares, and the **only** one `DoubleCheckPassword` treats as
/// a real failed attempt. Named because three call sites compare against it.
const CHECK_USER_PASSWORD_INVALID: &str = "api.user.check_user_password.invalid.app_error";

impl App {
    /// Port of `users.IsPasswordValidWithSettings` (app/users/password.go:18) wrapped in
    /// `App.IsPasswordValid` (authentication.go:51).
    ///
    /// # The id is built by concatenation, and the rules can stack
    ///
    /// The id starts at `model.user.is_valid.pwd` and each violated rule appends a suffix, so a
    /// password failing both the lowercase and the number rule is
    /// `model.user.is_valid.pwd_lowercase_number.app_error` — one id per *combination*, in the
    /// fixed order lowercase, uppercase, number, symbol. A port that returned the first failure
    /// would produce an id the client's translation table does not have.
    ///
    /// # Length short-circuits the character rules, but not each other
    ///
    /// `isMinMaxError` gates the four rule checks, so a too-short password is never *also*
    /// reported as missing a digit. The min and max checks do not gate one another — they cannot
    /// both fire, since no length is both below the minimum and above 72 unless the minimum
    /// exceeds 72, which `SetDefaults` does not prevent.
    ///
    /// Everything is counted in **bytes**. `SYMBOLS` includes a space.
    pub fn is_password_valid(&self, password: &str) -> AppResult {
        is_password_valid_with_settings(self.config(), password)
    }
}

/// Port of `users.IsPasswordValidWithSettings` (app/users/password.go:18), as a free function over
/// the settings.
///
/// Split from [`App::is_password_valid`] so the rules can be tested without a store: every branch
/// here is pure, and constructing an `App` to reach them needs a `PgPool`, which needs a Tokio
/// context a `#[test]` does not have.
fn is_password_valid_with_settings(config: &crate::config::Config, password: &str) -> AppResult {
    {
        let mut id = String::from("model.user.is_valid.pwd");
        let mut is_error = false;
        let mut is_min_max_error = false;

        if (password.len() as i64) < config.password_minimum_length {
            is_error = true;
            is_min_max_error = true;
            id.push_str("_min_length");
        }

        if password.len() > PASSWORD_MAXIMUM_LENGTH {
            is_error = true;
            is_min_max_error = true;
            id.push_str("_max_length");
        }

        if !is_min_max_error {
            if config.password_lowercase && !contains_any(password, LOWERCASE_LETTERS) {
                is_error = true;
                id.push_str("_lowercase");
            }
            if config.password_uppercase && !contains_any(password, UPPERCASE_LETTERS) {
                is_error = true;
                id.push_str("_uppercase");
            }
            if config.password_number && !contains_any(password, NUMBERS) {
                is_error = true;
                id.push_str("_number");
            }
            if config.password_symbol && !contains_any(password, SYMBOLS) {
                is_error = true;
                id.push_str("_symbol");
            }
        }

        if !is_error {
            return Ok(());
        }

        id.push_str(".app_error");
        // `Where` is **`User.IsValid`**, not `App.IsPasswordValid` — the id was borrowed from the
        // model validator and the `Where` came with it. Not wire-visible (`json:"-"`), but it
        // reaches the server log and costs nothing to keep right.
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "Min".to_owned(),
            serde_json::Value::from(config.password_minimum_length),
        );
        Err(Box::new(AppError::new(
            "User.IsValid",
            id,
            Some(params),
            String::new(),
            400,
        )))
    }
}

impl App {
    /// Port of `App.checkUserPassword` (authentication.go:65).
    ///
    /// # Three outcomes, and the middle one is a 500
    ///
    /// A mismatch is **401** `api.user.check_user_password.invalid.app_error`. An unreadable
    /// stored hash is **401** `…invalid_hash.app_error`. Anything else the hasher reports — a
    /// password over 72 bytes is the reachable one, because `CompareHashAndPassword` checks the
    /// byte cap before it compares — is **500** `app.valid_password_generic.app_error`. That last
    /// one matters more than it looks: [`App::double_check_password`] refunds the failed-attempt
    /// slot for every id *except* the mismatch, so an over-long password does not consume an
    /// attempt while a wrong one does.
    ///
    /// # Success can write to the database
    ///
    /// If the row was hashed with anything but the latest hasher it is silently re-hashed and
    /// updated — see [`App::migrate_password`]. So a *successful* password check is not a read.
    async fn check_user_password(&self, user: &User, password: &str) -> AppResult {
        if user.password.is_empty() || password.is_empty() {
            return Err(invalid_password(&user.id));
        }

        let (hasher, phc) = match password::get_hasher_from_phc_string(&user.password) {
            Ok(pair) => pair,
            Err(_) => {
                return Err(AppError::boxed(
                    "checkUserPassword",
                    "api.user.check_user_password.invalid_hash.app_error",
                    None,
                    format!("user_id={}", user.id),
                    401,
                ));
            }
        };

        match hasher.compare_hash_and_password(&phc, password) {
            Ok(()) => {}
            Err(password::CompareError::Mismatched) => return Err(invalid_password(&user.id)),
            Err(_) => {
                return Err(AppError::boxed(
                    "checkUserPassword",
                    "app.valid_password_generic.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        if !password::is_latest_hasher(&hasher) {
            return self.migrate_password(user, password).await;
        }

        Ok(())
    }

    /// Port of `App.migratePassword` (authentication.go:92).
    ///
    /// Re-hashes a verified password with the latest hasher and writes it. Note this goes through
    /// [`mm_store::UserStore::update_password`], which also clears `AuthData`, `AuthService` and
    /// `FailedAttempts` — so an old bcrypt row logging in successfully is, as a side effect,
    /// converted to email auth. That is Go's behaviour and not an accident of this port: the same
    /// store function serves both.
    async fn migrate_password(&self, user: &User, password: &str) -> AppResult {
        let new_hash = password::hash(password).map_err(|err| {
            tracing::error!(error = %err, "re-hashing a verified password failed");
            AppError::boxed(
                "migratePassword",
                "app.user.check_user_password.failed_migration",
                None,
                String::new(),
                500,
            )
        })?;

        self.store()
            .user()
            .update_password(&user.id, &new_hash)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "storing a migrated password failed");
                AppError::boxed(
                    "migratePassword",
                    "app.user.check_user_password.failed_update",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // Go's `a.InvalidateCacheForUser` — there is no user cache here. The *Go* server's cache
        // is now stale for this row; see [D-190].
        Ok(())
    }

    /// Port of `App.DoubleCheckPassword` (authentication.go:167) — the check for a caller who is
    /// already logged in.
    ///
    /// # The order of the four steps is the behaviour
    ///
    /// 1. **Claim** a failed-attempt slot, conditionally on being below `MaximumLoginAttempts`.
    /// 2. If the claim failed, refuse with `api.user.check_user_login_attempts.too_many.app_error`
    ///    — **before the password is looked at**, so a correct password cannot unlock an account
    ///    that is already at the cap.
    /// 3. Check the password; refund the slot for every failure *except* a credential mismatch,
    ///    so a backend fault or an over-long password cannot lock anybody out.
    /// 4. On success, zero the counter.
    ///
    /// A reader reordering 1 and 3 gets a version that is correct on every happy path and wrong
    /// under exactly the concurrency the claim exists for.
    ///
    /// Note `Where` on the lockout error is **`checkUserLoginAttempts`**, not this function —
    /// Go reuses the free function's name for an error it builds inline.
    pub async fn double_check_password(&self, user: &User, password: &str) -> AppResult {
        let max_attempts = clamp_attempts(self.config().maximum_login_attempts);

        let claimed = self
            .store()
            .user()
            .try_increment_failed_password_attempts(&user.id, max_attempts)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "claiming a failed-attempt slot failed");
                attempts_error("DoubleCheckPassword")
            })?;

        if !claimed {
            return Err(AppError::boxed(
                "checkUserLoginAttempts",
                "api.user.check_user_login_attempts.too_many.app_error",
                None,
                format!("user_id={}", user.id),
                401,
            ));
        }

        if let Err(err) = self.check_user_password(user, password).await {
            if err.id != CHECK_USER_PASSWORD_INVALID
                && let Err(refund_err) = self
                    .store()
                    .user()
                    .decrement_failed_password_attempts(&user.id)
                    .await
            {
                tracing::warn!(error = %refund_err, user_id = %user.id, "failed to refund login attempt slot");
            }
            return Err(err);
        }

        self.store()
            .user()
            .update_failed_password_attempts(&user.id, 0)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "clearing the failed-attempt counter failed");
                attempts_error("DoubleCheckPassword")
            })?;

        Ok(())
    }

    /// Port of `App.UpdatePassword` (user.go:1758).
    ///
    /// Validates, refuses remote and magic-link accounts, hashes, writes, and then — only when
    /// `ServiceSettings.TerminateSessionsOnPasswordChange` is on — revokes every session of the
    /// user **except the caller's own**.
    ///
    /// `caller_session` is Go's `rctx.Session()`. It is consulted for one thing: whether the
    /// session making the request belongs to the user whose password is changing, in which case
    /// it survives. An admin changing somebody else's password matches nothing and logs that user
    /// out everywhere, which is the point.
    ///
    /// **The revoke loop stops at the first failure and answers 500**, leaving the earlier
    /// revocations done and the rest standing — Go returns from inside the loop. A port that
    /// collected errors and continued would log out sessions Go leaves alive.
    pub async fn update_password(
        &self,
        caller_session: Option<&Session>,
        user: &User,
        new_password: &str,
    ) -> AppResult {
        self.is_password_valid(new_password)?;

        if user.is_remote() {
            return Err(update_password_failed());
        }

        if user.is_magic_link_enabled() {
            return Err(AppError::boxed(
                "UpdatePassword",
                "api.user.update_password.magic_link.app_error",
                None,
                String::new(),
                400,
            ));
        }

        let hashed = password::hash(new_password).map_err(|err| {
            // Go's comment: "can't be password length (checked in IsPasswordValid)". True only
            // while `MinimumLength <= 72`; the hasher's cap is the same 72, so the branch is
            // unreachable on a sane config and a 500 on an insane one.
            AppError::boxed(
                "UpdatePassword",
                "api.user.update_password.password_hash.app_error",
                None,
                format!("user_id={} {err}", user.id),
                500,
            )
        })?;

        self.store()
            .user()
            .update_password(&user.id, &hashed)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "storing the new password failed");
                update_password_failed()
            })?;

        if !self.config().terminate_sessions_on_password_change {
            return Ok(());
        }

        let current_session_id = caller_session
            .filter(|session| session.user_id == user.id)
            .map(|session| session.id.as_str())
            .unwrap_or_default();

        let sessions = self.get_sessions(&user.id).await.map_err(|err| {
            tracing::error!(error = %err, "listing sessions to revoke failed");
            update_password_failed()
        })?;

        for session in sessions {
            if session.id == current_session_id {
                continue;
            }
            self.revoke_session(&session).await.map_err(|err| {
                tracing::error!(error = %err, "revoking a session after a password change failed");
                update_password_failed()
            })?;
        }

        Ok(())
    }

    /// Port of `App.UpdatePasswordSendEmail` (user.go:1812).
    ///
    /// The e-mail is a `Srv().Go(...)` goroutine whose failure Go only logs, so it is not part of
    /// the response either way — but it *is* part of what a user experiences, and it is not sent
    /// here. See [D-235]. The `method` string Go passes is the translated body of that e-mail and
    /// has no other reader, so it is not a parameter of this function.
    pub async fn update_password_send_email(
        &self,
        caller_session: Option<&Session>,
        user: &User,
        new_password: &str,
    ) -> AppResult {
        self.update_password(caller_session, user, new_password)
            .await?;
        tracing::warn!(
            user_id = %user.id,
            "password changed; the password-change e-mail Go sends here is not ported (D-235)"
        );
        Ok(())
    }

    /// Port of `App.UpdatePasswordAsUser` (user.go:1142) — the self-service path.
    ///
    /// # Four refusals before the password is checked, and their order is observable
    ///
    /// missing account → SSO account → magic-link account → current password. So a SAML user who
    /// also types the wrong current password is told they are a SAML user, and **no failed
    /// attempt is recorded** for them. `IsSSOUser` is not what is tested here: Go checks
    /// `AuthData != nil && *AuthData != ""`, which is the same set by construction but reached a
    /// different way, and the error id says `oauth` while the detail says `auth_service=`.
    ///
    /// # The mismatch id is rewritten
    ///
    /// `DoubleCheckPassword`'s **401** `api.user.check_user_password.invalid.app_error` becomes a
    /// **400** `api.user.update_password.incorrect.app_error` here — status and id both. Every
    /// other error from it, the lockout included, passes through untouched, so a locked-out user
    /// changing their own password gets a 401 while a wrong password gets a 400.
    pub async fn update_password_as_user(
        &self,
        caller_session: Option<&Session>,
        user_id: &str,
        current_password: &str,
        new_password: &str,
    ) -> AppResult {
        let user = self.get_user(user_id).await?;

        if user.auth_data.as_deref().unwrap_or_default() != "" {
            return Err(AppError::boxed(
                "updatePassword",
                "api.user.update_password.oauth.app_error",
                None,
                format!("auth_service={}", user.auth_service),
                400,
            ));
        }

        if user.is_magic_link_enabled() {
            return Err(AppError::boxed(
                "updatePassword",
                "api.user.update_password.magic_link.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if let Err(err) = self.double_check_password(&user, current_password).await {
            if err.id == CHECK_USER_PASSWORD_INVALID {
                return Err(AppError::boxed(
                    "updatePassword",
                    "api.user.update_password.incorrect.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }
            return Err(err);
        }

        self.update_password_send_email(caller_session, &user, new_password)
            .await
    }

    /// Port of `App.UpdatePasswordByUserIdSendEmail` (user.go:1749) — the admin path.
    ///
    /// No current password, no failed-attempt bookkeeping: the caller's rights were checked at
    /// the handler. Note it does **not** refuse an SSO account the way the self path does, so an
    /// admin can give a SAML user a password — and `UpdatePassword`'s store call then clears
    /// `AuthService`, converting the account.
    pub async fn update_password_by_user_id_send_email(
        &self,
        caller_session: Option<&Session>,
        user_id: &str,
        new_password: &str,
    ) -> AppResult {
        let user = self.get_user(user_id).await?;
        self.update_password_send_email(caller_session, &user, new_password)
            .await
    }

    /// Port of `App.UpdateHashedPasswordByUserId` (user.go:1826) and `UpdateHashedPassword`
    /// (user.go:1835) — the `already_hashed=true` branch.
    ///
    /// **Nothing validates the string.** It is written to `Users.Password` verbatim, so an admin
    /// can plant a hash from another install — which is what the branch exists for. There is no
    /// `IsPasswordValid`, no hashing, and no session termination; only the remote-user refusal
    /// survives, and its `Where` is `UpdatePassword` because Go copied the error.
    pub async fn update_hashed_password_by_user_id(
        &self,
        user_id: &str,
        new_hashed_password: &str,
    ) -> AppResult {
        let user = self.get_user(user_id).await?;

        if user.is_remote() {
            return Err(update_password_failed());
        }

        self.store()
            .user()
            .update_password(&user.id, new_hashed_password)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "storing a pre-hashed password failed");
                update_password_failed()
            })?;

        Ok(())
    }

    /// Port of `App.GetPasswordRecoveryToken` (user.go:2004).
    ///
    /// A missing token and a token of the wrong type are both **400**, with different ids —
    /// `api.user.reset_password.invalid_link.app_error` and
    /// `…broken_token.app_error`. Neither is a 404: the route must not confirm that a token
    /// string exists.
    pub async fn get_password_recovery_token(&self, token: &str) -> AppResult<Token> {
        let token = self
            .store()
            .token()
            .get_by_token(token)
            .await
            .map_err(|_| {
                AppError::boxed(
                    "GetPasswordRecoveryToken",
                    "api.user.reset_password.invalid_link.app_error",
                    None,
                    String::new(),
                    400,
                )
            })?;

        if token.type_ != TOKEN_TYPE_PASSWORD_RECOVERY {
            return Err(AppError::boxed(
                "GetPasswordRecoveryToken",
                "api.user.reset_password.broken_token.app_error",
                None,
                String::new(),
                400,
            ));
        }

        Ok(token)
    }

    /// Port of `App.GetVerifyEmailToken` (user.go:2357) — the same shape, different ids.
    pub async fn get_verify_email_token(&self, token: &str) -> AppResult<Token> {
        let token = self
            .store()
            .token()
            .get_by_token(token)
            .await
            .map_err(|_| {
                AppError::boxed(
                    "GetVerifyEmailToken",
                    "api.user.verify_email.bad_link.app_error",
                    None,
                    String::new(),
                    400,
                )
            })?;

        if token.type_ != TOKEN_TYPE_VERIFY_EMAIL {
            return Err(AppError::boxed(
                "GetVerifyEmailToken",
                "api.user.verify_email.broken_token.app_error",
                None,
                String::new(),
                400,
            ));
        }

        Ok(token)
    }

    /// Port of `App.DeleteToken` (user.go:2048).
    pub async fn delete_token(&self, token: &Token) -> AppResult {
        self.store()
            .token()
            .delete(&token.token)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "deleting a one-shot token failed");
                AppError::boxed(
                    "DeleteToken",
                    "app.recover.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.resetPasswordFromToken` (user.go:1854).
    ///
    /// # The expiry is recomputed rather than delegated
    ///
    /// Go explicitly does **not** call `token.IsExpired()` here — its comment says so — and
    /// compares `now > CreateAt + PasswordRecoverExpiryTime` against a `nowMilli` it was handed.
    /// The two agree for a `password_recovery` token, since that is the window `IsExpired`
    /// picks for the type; the distinction exists so the function is testable, and the id it
    /// produces (`api.user.reset_password.link_expired.app_error`) is the same one an
    /// e-mail-mismatch produces, two checks later.
    ///
    /// # The token is deleted only on success, and a failed delete is not an error
    ///
    /// Every refusal above leaves the row in place — so a reset that fails validation can be
    /// retried with the same link. The delete happens after the password is already written, and
    /// its failure is logged and swallowed: the password change has committed and reporting a 500
    /// would tell the user the opposite of what happened.
    ///
    /// # `Extra` is Go-cased JSON
    ///
    /// `{"UserId":"…","Email":"…"}` — Go marshals an anonymous struct with no tags, so the keys
    /// are the **Go field names**. A port that used the wire casing (`user_id`) would parse every
    /// live token to the zero value and answer 404 for all of them.
    pub async fn reset_password_from_token(
        &self,
        caller_session: Option<&Session>,
        user_supplied_token: &str,
        new_password: &str,
    ) -> AppResult {
        self.reset_password_from_token_at(
            caller_session,
            user_supplied_token,
            new_password,
            get_millis(),
        )
        .await
    }

    /// [`App::reset_password_from_token`] with the clock injected — Go's `nowMilli` parameter.
    pub async fn reset_password_from_token_at(
        &self,
        caller_session: Option<&Session>,
        user_supplied_token: &str,
        new_password: &str,
        now_milli: i64,
    ) -> AppResult {
        let token = self
            .get_password_recovery_token(user_supplied_token)
            .await?;

        if now_milli > token.create_at + mm_model::token::PASSWORD_RECOVER_EXPIRY_TIME {
            return Err(link_expired());
        }

        let data: TokenExtra = serde_json::from_str(&token.extra).map_err(|err| {
            tracing::warn!(error = %err, "a password-recovery token's Extra will not parse");
            AppError::boxed(
                "resetPassword",
                "api.user.reset_password.token_parse.error",
                None,
                String::new(),
                500,
            )
        })?;

        let user = self.get_user(&data.user_id).await?;

        if user.email != data.email {
            return Err(link_expired());
        }

        if user.is_sso_user() {
            return Err(AppError::boxed(
                "ResetPasswordFromCode",
                "api.user.reset_password.sso.app_error",
                None,
                format!("userId={}", user.id),
                400,
            ));
        }

        if user.is_magic_link_enabled() {
            return Err(AppError::boxed(
                "ResetPasswordFromCode",
                "api.user.send_password_reset.guest_magic_link.app_error",
                None,
                format!("userId={}", user.id),
                400,
            ));
        }

        if user.is_remote() {
            return Err(AppError::boxed(
                "resetPassword",
                "api.user.reset_password.broken_token.app_error",
                None,
                String::new(),
                400,
            ));
        }

        self.update_password_send_email(caller_session, &user, new_password)
            .await?;

        if let Err(err) = self.delete_token(&token).await {
            tracing::warn!(error = %err, "failed to delete token");
        }

        Ok(())
    }

    /// Port of `App.VerifyUserEmail` (user.go:2395).
    ///
    /// Writes the address and the flag, then re-reads the user and publishes `user_updated` —
    /// three of them, see [`App::send_updated_user_event`]. The re-read is not a nicety: the
    /// event must carry the *new* email, and the caller's copy predates the write.
    pub async fn verify_user_email(&self, user_id: &str, email: &str) -> AppResult {
        self.store()
            .user()
            .verify_email(user_id, email)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "marking an email verified failed");
                AppError::boxed(
                    "VerifyUserEmail",
                    "app.user.verify_email.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let user = self.get_user(user_id).await?;
        self.send_updated_user_event(&user).await;
        Ok(())
    }

    /// Port of `App.VerifyEmailFromToken` (user.go:2313).
    ///
    /// # `IsExpired` here, arithmetic in the password twin
    ///
    /// This one *does* call `token.IsExpired()`, which for `verify_email` borrows the
    /// **password-recovery** 24-hour window rather than the 48-hour default. The two functions
    /// reach the same number by different routes.
    ///
    /// # The email is lower-cased before the write and compared after it
    ///
    /// `tokenData.Email` is lower-cased, then written, and only *then* compared with the user's
    /// stored address to decide whether this was an email *change* worth notifying about. So the
    /// write happens either way and the comparison is against the pre-write copy.
    ///
    /// # The user is fetched before the write and its error is not remapped
    ///
    /// A token naming a deleted user produces `get_user`'s own 404 — which the handler then
    /// wraps into a 400 `api.user.verify_email.bad_link.app_error`, because `verifyUserEmail`
    /// wraps *every* error from here.
    pub async fn verify_email_from_token(&self, user_supplied_token: &str) -> AppResult {
        let token = self.get_verify_email_token(user_supplied_token).await?;

        if token.is_expired() {
            return Err(AppError::boxed(
                "VerifyEmailFromToken",
                "api.user.verify_email.link_expired.app_error",
                None,
                String::new(),
                400,
            ));
        }

        let data: TokenExtra = serde_json::from_str(&token.extra).map_err(|err| {
            tracing::warn!(error = %err, "an email-verification token's Extra will not parse");
            AppError::boxed(
                "VerifyEmailFromToken",
                "api.user.verify_email.token_parse.error",
                None,
                String::new(),
                500,
            )
        })?;

        let user = self.get_user(&data.user_id).await?;
        let email = data.email.to_lowercase();

        self.verify_user_email(&data.user_id, &email).await?;

        if user.email != email {
            // Go's `SendEmailChangeEmail` goroutine. Not ported — see [D-235].
            tracing::warn!(
                user_id = %user.id,
                "email changed by verification; the change notification Go sends is not ported (D-235)"
            );
        }

        if let Err(err) = self.delete_token(&token).await {
            tracing::warn!(error = %err, "failed to delete token");
        }

        Ok(())
    }

    /// Port of `App.ResetPasswordFailedAttempts` (user.go:3330).
    ///
    /// One store write and one error id. It does **not** go through
    /// `mm_store::UserStore::update_password`, so the account's auth service is untouched — this
    /// only unlocks.
    pub async fn reset_password_failed_attempts(&self, user: &User) -> AppResult {
        self.store()
            .user()
            .update_failed_password_attempts(&user.id, 0)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "clearing the failed-attempt counter failed");
                AppError::boxed(
                    "ResetPasswordFailedAttempts",
                    "app.user.reset_password_failed_attempts.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.GetSessionById` (session.go:352).
    ///
    /// A miss is **400** `app.session.get.app_error`, not a 404 and not a 401 — the id is the
    /// same one the store failure produces, so a caller cannot tell a nonexistent session from a
    /// broken query.
    ///
    /// The store's lookup matches `Id = $1 OR Token = $1`, which Go's does too; unlike
    /// [`App::get_session`] there is no follow-up check that the argument was the id, so a
    /// **token** works here. That is reachable: `logout` passes `Session().Id`, but nothing
    /// prevents another caller from passing a token.
    pub async fn get_session_by_id(&self, session_id: &str) -> AppResult<Session> {
        self.store().session().get(session_id).await.map_err(|err| {
            tracing::debug!(error = %err, "session lookup by id failed");
            AppError::boxed(
                "GetSessionById",
                "app.session.get.app_error",
                None,
                String::new(),
                400,
            )
        })
    }

    /// Port of `App.RevokeSessionById` (session.go:361).
    pub async fn revoke_session_by_id(&self, session_id: &str) -> AppResult {
        let session = self.get_session_by_id(session_id).await?;
        self.revoke_session(&session).await
    }

    /// Port of `App.RevokeSession` (session.go:370) over `PlatformService.RevokeSession`
    /// (platform/session.go:227).
    ///
    /// # The OAuth branch is not ported
    ///
    /// `session.IsOAuth` sends Go down `RevokeAccessToken`, which also reads and deletes the
    /// `OAuthAccessData` row and fails with `GetTokenError` when there is none. There is no
    /// access-data store here, so a caller holding an OAuth session must be forwarded to Go
    /// rather than answered — every caller in this crate checks
    /// [`Session::is_oauth`](mm_model::session::Session) first. Deleting only the session row
    /// would leave the access data behind and the token replayable.
    ///
    /// Both failure modes in Go's `switch` produce the **same** id and status, which is why there
    /// is one arm here.
    ///
    /// The mobile wipe signal (`sendMobileWipeSignal`) is a push notification behind
    /// `MobileEphemeralModeSettings.Enable`, off by default and unported.
    pub async fn revoke_session(&self, session: &Session) -> AppResult {
        if session.is_oauth {
            // Unreachable from any handler in this crate; see the note above. Kept as an explicit
            // refusal rather than a silent fall-through to the non-OAuth path.
            return Err(AppError::boxed(
                "RevokeSession",
                "app.session.remove.app_error",
                None,
                "oauth session revocation is not ported".to_owned(),
                500,
            ));
        }

        self.store()
            .session()
            .remove(&session.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "removing a session failed");
                AppError::boxed(
                    "RevokeSession",
                    "app.session.remove.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

/// The `{UserId, Email}` anonymous struct Go marshals into `Token.Extra` for both the
/// password-recovery and the email-verification token.
///
/// **Go field names, not wire names** — the struct has no `json:` tags, so `encoding/json` uses
/// the identifiers verbatim. Go's decoder is also case-insensitive on the way back, which this is
/// not; every token either server mints carries the exact casing below, so the difference is only
/// reachable with a hand-written row.
#[derive(Debug, serde::Deserialize)]
struct TokenExtra {
    #[serde(rename = "UserId", default)]
    user_id: String,
    #[serde(rename = "Email", default)]
    email: String,
}

/// Port of `strings.ContainsAny` — true when any **rune** of `chars` appears in `s`.
fn contains_any(s: &str, chars: &str) -> bool {
    s.chars().any(|c| chars.contains(c))
}

/// `MaximumLoginAttempts` is an `int` in Go and an `i32` column here.
///
/// A configured value beyond `i32` saturates rather than wrapping: wrapping a large positive
/// setting to a negative one would make `FailedAttempts < max` false for every account and lock
/// the whole server out, which is the opposite of what the operator asked for.
fn clamp_attempts(max_attempts: i64) -> i32 {
    max_attempts.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn invalid_password(user_id: &str) -> Box<AppError> {
    AppError::boxed(
        "checkUserPassword",
        CHECK_USER_PASSWORD_INVALID,
        None,
        format!("user_id={user_id}"),
        401,
    )
}

fn attempts_error(whence: &'static str) -> Box<AppError> {
    AppError::boxed(
        whence,
        "app.user.update_failed_pwd_attempts.app_error",
        None,
        String::new(),
        500,
    )
}

fn update_password_failed() -> Box<AppError> {
    AppError::boxed(
        "UpdatePassword",
        "api.user.update_password.failed.app_error",
        None,
        String::new(),
        500,
    )
}

fn link_expired() -> Box<AppError> {
    AppError::boxed(
        "resetPassword",
        "api.user.reset_password.link_expired.app_error",
        None,
        String::new(),
        400,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The rules, over a bare `Config`. No `App`, and therefore no pool: `App::with_config` builds
    /// a `PgPool`, and `connect_lazy` panics with "this functionality requires a Tokio context"
    /// inside a plain `#[test]`. Measured — five of these tests failed that way before
    /// [`is_password_valid_with_settings`] was split out.
    struct Rules(Config);

    impl Rules {
        fn is_password_valid(&self, password: &str) -> AppResult {
            is_password_valid_with_settings(&self.0, password)
        }
    }

    fn app_with(config: Config) -> Rules {
        Rules(config)
    }

    fn strict() -> Config {
        Config {
            password_minimum_length: 8,
            password_lowercase: true,
            password_uppercase: true,
            password_number: true,
            password_symbol: true,
            ..Config::default()
        }
    }

    #[test]
    fn the_default_settings_accept_any_eight_bytes() {
        let app = app_with(Config::default());
        assert!(app.is_password_valid("aaaaaaaa").is_ok());
        assert!(app.is_password_valid("12345678").is_ok());
    }

    #[test]
    fn too_short_is_min_length_and_the_character_rules_do_not_also_fire() {
        let app = app_with(strict());
        let err = app
            .is_password_valid("aA1!")
            .expect_err("four bytes is short");
        assert_eq!(err.id, "model.user.is_valid.pwd_min_length.app_error");
        assert_eq!(err.status_code, 400);
    }

    /// The character rules **stack** into one id, in Go's fixed order.
    #[test]
    fn several_violated_rules_concatenate_in_order() {
        let app = app_with(strict());
        let err = app
            .is_password_valid("!!!!!!!!")
            .expect_err("no letters, no digits");
        assert_eq!(
            err.id,
            "model.user.is_valid.pwd_lowercase_uppercase_number.app_error"
        );
    }

    #[test]
    fn each_rule_can_fire_alone() {
        let app = app_with(strict());
        for (password, expected) in [
            ("AAAAAAA1!", "model.user.is_valid.pwd_lowercase.app_error"),
            ("aaaaaaa1!", "model.user.is_valid.pwd_uppercase.app_error"),
            ("aaaaaaaA!", "model.user.is_valid.pwd_number.app_error"),
            ("aaaaaaaA1", "model.user.is_valid.pwd_symbol.app_error"),
        ] {
            let err = app.is_password_valid(password).expect_err(password);
            assert_eq!(err.id, expected, "for {password}");
        }
    }

    /// A space is in `model.SYMBOLS`, so it satisfies the symbol rule.
    #[test]
    fn a_space_counts_as_a_symbol() {
        let app = app_with(strict());
        assert!(app.is_password_valid("aaaaaA1 ").is_ok());
    }

    /// Over 72 **bytes**, and the rules do not also fire.
    #[test]
    fn too_long_is_max_length_and_counted_in_bytes() {
        let app = app_with(strict());
        let err = app
            .is_password_valid(&"a".repeat(73))
            .expect_err("73 bytes");
        assert_eq!(err.id, "model.user.is_valid.pwd_max_length.app_error");

        // 24 four-byte emoji is 96 bytes and 24 characters — refused for length, which a
        // rune-counting port would accept.
        let emoji = "\u{1F600}".repeat(24);
        assert_eq!(emoji.chars().count(), 24);
        let err = app.is_password_valid(&emoji).expect_err("96 bytes");
        assert_eq!(err.id, "model.user.is_valid.pwd_max_length.app_error");
    }

    /// Exactly 72 bytes is accepted — the comparison is `>`, not `>=`.
    #[test]
    fn seventy_two_bytes_is_the_last_accepted_length() {
        let app = app_with(Config {
            password_minimum_length: 1,
            ..Config::default()
        });
        assert!(app.is_password_valid(&"a".repeat(72)).is_ok());
        assert!(app.is_password_valid(&"a".repeat(73)).is_err());
    }

    /// A minimum of zero accepts the empty string: the comparison is `<`, so `0 < 0` is false.
    #[test]
    fn a_minimum_of_zero_accepts_an_empty_password() {
        let app = app_with(Config {
            password_minimum_length: 0,
            ..Config::default()
        });
        assert!(app.is_password_valid("").is_ok());
    }

    #[test]
    fn contains_any_matches_gos_rune_semantics() {
        assert!(contains_any("abc", "cde"));
        assert!(!contains_any("abc", "def"));
        assert!(!contains_any("abc", ""));
        assert!(!contains_any("", "abc"));
    }

    #[test]
    fn clamp_attempts_saturates_rather_than_wrapping() {
        assert_eq!(clamp_attempts(10), 10);
        assert_eq!(clamp_attempts(i64::from(i32::MAX) + 1), i32::MAX);
        assert_eq!(clamp_attempts(-1), -1);
    }

    /// Go's field names, not the wire's. A `user_id` key must not parse.
    #[test]
    fn token_extra_is_go_cased() {
        let parsed: TokenExtra =
            serde_json::from_str(r#"{"UserId":"abc","Email":"A@B.c"}"#).expect("parses");
        assert_eq!(parsed.user_id, "abc");
        assert_eq!(parsed.email, "A@B.c");

        let wrong: TokenExtra =
            serde_json::from_str(r#"{"user_id":"abc","email":"a@b.c"}"#).expect("parses, emptily");
        assert_eq!(wrong.user_id, "");
        assert_eq!(wrong.email, "");
    }
}
