//! Port of `channels/app/login.go` and the read half of `channels/app/authentication.go` — the
//! app layer behind `POST /api/v4/users/login`.
//!
//! # What is here and what the handler forwards
//!
//! Everything in this module is the **local-password** path: resolve a login id to a row, claim a
//! failed-attempt slot, check the password, create a session. Four branches of Go's `login` are
//! not here at all, and `mm_api::login` detects each of them and hands the request to the Go
//! server **before** anything in this module runs:
//!
//! | Branch | Detected by | Why it is not here |
//! |---|---|---|
//! | guest magic link | `magic_link_token` in the body | needs `AuthenticateUserForGuestMagicLink` and a licence |
//! | LDAP | `LdapSettings.Enable` | needs an LDAP client |
//! | MFA | `user.MfaActive && EnableMultifactorAuthentication` | needs `platform/shared/mfa` |
//! | CWS | unreachable from this route | `login` passes `cwsToken` as `""` (api4/user.go:2229) |
//!
//! The MFA one is the delicate case. Go checks MFA **after** claiming a failed-attempt slot and
//! after verifying the password (`CheckPasswordAndAllCriteria`, authentication.go:127-153), so a
//! forward taken at that point would leave the counter moved and let Go move it again. The
//! handler therefore decides before any write. See `mm_api::login`.
//!
//! # The counter is the state this file is really about
//!
//! `Users.FailedAttempts` is one column shared by both servers and by three call paths
//! (`login`, `PUT /users/{id}/password`, `POST /users/{id}/reset_failed_attempts`). The claim
//! ordering — increment first, conditionally on being under the cap, and refund when the failure
//! turns out not to be a credential mismatch — is in
//! [`crate::App::double_check_password`]'s doc comment and is reproduced here for the login path.

use mm_model::session::{
    LoginOptions, SESSION_PROP_BROWSER, SESSION_PROP_IS_GUEST, SESSION_PROP_OS,
    SESSION_PROP_PLATFORM, Session, is_valid_standard_device_id, is_valid_voip_device_id,
};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, StringMap};
use mm_store::{SessionStore, UserStore};

use crate::App;
use crate::auth::{CHECK_USER_PASSWORD_INVALID, attempts_error, clamp_attempts};
use crate::user_agent;

/// `maxSessionsLimit` (app/session.go:24). MM-55320.
const MAX_SESSIONS_LIMIT: i64 = 500;

/// `returnLimit` inside `limitNumberOfSessions` (app/session.go:156).
const LIMIT_SESSIONS_RETURN_LIMIT: i64 = 100;

impl App {
    /// Port of `App.GetUserForLogin` (app/login.go:94).
    ///
    /// # An explicit `id` wins outright, and its failure is **not** the same error
    ///
    /// When the body carries `id`, Go looks the user up by primary key and returns whatever that
    /// says — a miss is `app.user.missing_account.const` at **400** (the status is rewritten from
    /// the 404 `GetUser` gives), and anything else is a 500. It never falls through to the
    /// login-id lookup, so a body with a wrong `id` and a right `login_id` fails.
    ///
    /// # The login-id lookup swallows its error
    ///
    /// `if user, err := ...GetForLogin(...); err == nil` — a failure is discarded, not returned,
    /// so "no such user", "two users matched" and "the database is down" are indistinguishable
    /// here and all three end at the single `store.sql_user.get_for_login.app_error` **400**
    /// below. That is deliberate on Go's part: this function is an account-enumeration surface
    /// and the handler's error mask is the second layer over the same concern.
    ///
    /// # Both flags off skips the lookup entirely
    ///
    /// The whole block is guarded by `enableEmail || enableUsername`, so with both off nothing is
    /// queried and every login fails with the same 400 — which is how an administrator turns
    /// local logins off. [`mm_store::UserStore::get_for_login`] refuses that combination too; the
    /// guard here means it is never reached from this path.
    #[tracing::instrument(skip_all, fields(by_id = !id.is_empty(), found))]
    pub async fn get_user_for_login(&self, id: &str, login_id: &str) -> AppResult<User> {
        let enable_username = self.config().enable_sign_in_with_username;
        let enable_email = self.config().enable_sign_in_with_email;

        if enable_email || enable_username {
            if !id.is_empty() {
                return match self.get_user(id).await {
                    Ok(user) => {
                        tracing::Span::current().record("found", true);
                        Ok(user)
                    }
                    Err(mut err) => {
                        // Go rewrites the status in place: 404 becomes **400** for the missing
                        // account, and everything else becomes 500. Both are the *same*
                        // `AppError` object with a different `StatusCode`, so the id a client
                        // sees is `app.user.missing_account.const` at 400.
                        err.status_code = if err.id == "app.user.missing_account.const" {
                            400
                        } else {
                            500
                        };
                        tracing::Span::current().record("found", false);
                        Err(err)
                    }
                };
            }

            if let Ok(user) = self
                .store()
                .user()
                .get_for_login(login_id, enable_username, enable_email)
                .await
            {
                tracing::Span::current().record("found", true);
                return Ok(user);
            }
        }

        // Go's LDAP fallback sits between here and the refusal. It is not ported; the handler
        // forwards while `LdapSettings.Enable` is on, so this point is only reached with LDAP off
        // — where Go's `if *a.Config().LdapSettings.Enable && a.Ldap() != nil` is false too.
        tracing::Span::current().record("found", false);
        Err(AppError::boxed(
            "GetUserForLogin",
            "store.sql_user.get_for_login.app_error",
            None,
            String::new(),
            400,
        ))
    }

    /// Whether a login for this body would need the MFA machinery this port does not have.
    ///
    /// **Read-only, and that is the whole point.** `CheckUserMfa` is consulted by Go *after* a
    /// failed-attempt slot has been claimed and the password verified, so a handler that
    /// discovered it needed to forward at that moment would already have moved a counter Go is
    /// about to move again. This asks the same question — `MfaActive && EnableMultifactorAuthentication`
    /// — before anything is written, at the cost of one extra `SELECT`.
    ///
    /// A login id that resolves to nothing answers `false`: there is no user to have MFA, and the
    /// refusal that follows is identical on both servers.
    #[tracing::instrument(skip_all, fields(needs_mfa))]
    pub async fn login_needs_mfa(&self, id: &str, login_id: &str) -> bool {
        if !self.config().enable_multifactor_authentication {
            return false;
        }
        let needs = matches!(
            self.get_user_for_login(id, login_id).await,
            Ok(user) if user.mfa_active
        );
        tracing::Span::current().record("needs_mfa", needs);
        needs
    }

    /// Port of `App.AuthenticateUserForLogin` (app/login.go:29), minus the CWS branch.
    ///
    /// # A blank password is refused before the account is looked up
    ///
    /// `api.user.login.blank_pwd.app_error` at **400**, and it is one of the ids the handler's
    /// mask lets through — so a client that sends no password learns that, while a client that
    /// sends a wrong one does not learn anything. The check precedes `GetUserForLogin`, so a
    /// blank password against a nonexistent account is still `blank_pwd` and not the lookup
    /// error.
    ///
    /// The `cwsToken` parameter is gone rather than passed as `""`: `isCWSLogin` is
    /// `License().IsCloud() && token != ""` and this route always passes the empty string, so the
    /// branch is unreachable from here (api4/user.go:2229).
    #[tracing::instrument(skip_all)]
    pub async fn authenticate_user_for_login(
        &self,
        id: &str,
        login_id: &str,
        password: &str,
        mfa_token: &str,
    ) -> AppResult<User> {
        if password.is_empty() {
            return Err(AppError::boxed(
                "AuthenticateUserForLogin",
                "api.user.login.blank_pwd.app_error",
                None,
                String::new(),
                400,
            ));
        }

        let user = self.get_user_for_login(id, login_id).await?;
        self.authenticate_user(user, password, mfa_token).await
    }

    /// Port of `App.authenticateUser` (authentication.go:450), local-password arm only.
    ///
    /// # `AuthService` decides which arm, and neither of the other two is served here
    ///
    /// An LDAP row goes to `checkLdapUserPasswordAndAllCriteria`; any other non-empty
    /// `AuthService` is refused with `api.user.login.use_auth_service.app_error` at 400, whose
    /// `AuthService` parameter is upper-cased for SAML alone. Both are reproduced as refusals
    /// rather than as forwards, because by the time they are reachable the handler has already
    /// established that LDAP is off — under which Go's own LDAP arm answers
    /// `api.user.login_ldap.not_available.app_error` at **501** and never touches the counter.
    ///
    /// All three of those ids are masked by the handler, so a client sees the same
    /// `invalid_credentials_*` refusal it would get for a wrong password. The ids still matter:
    /// they are what the *server log* records, and `use_auth_service` is the one a support
    /// engineer needs.
    ///
    /// Note what does **not** happen on the non-local arms: no failed-attempt slot is claimed, so
    /// an SSO account cannot be locked out by repeated password guesses at this endpoint.
    async fn authenticate_user(
        &self,
        user: User,
        password: &str,
        mfa_token: &str,
    ) -> AppResult<User> {
        if user.auth_service == mm_model::user::external::USER_AUTH_SERVICE_LDAP {
            // `ldapAvailable` is `Enable && Ldap() != nil && license != nil && license.LDAP`.
            // There is no LDAP client here, so the middle term is false whatever the config says
            // and this arm is the only reachable one. Go rewrites the status to 401 on the way
            // out of `checkLdapUserPasswordAndAllCriteria`, but not on this branch — 501 stands.
            return Err(AppError::boxed(
                "login",
                "api.user.login_ldap.not_available.app_error",
                None,
                String::new(),
                501,
            ));
        }

        if !user.auth_service.is_empty() {
            let auth_service =
                if user.auth_service == mm_model::user::external::USER_AUTH_SERVICE_SAML {
                    user.auth_service.to_uppercase()
                } else {
                    user.auth_service.clone()
                };
            return Err(AppError::boxed(
                "login",
                "api.user.login.use_auth_service.app_error",
                Some(std::collections::HashMap::from([(
                    "AuthService".to_owned(),
                    serde_json::Value::String(auth_service),
                )])),
                String::new(),
                400,
            ));
        }

        if let Err(mut err) = self
            .check_password_and_all_criteria(&user.id, password, mfa_token)
            .await
        {
            // `err.StatusCode = http.StatusUnauthorized` — unconditional, so even the 500s from
            // the counter writes leave this function as 401s.
            err.status_code = 401;
            return Err(err);
        }

        Ok(user)
    }

    /// Port of `App.CheckPasswordAndAllCriteria` (authentication.go:110).
    ///
    /// # Seven steps, and the order is the behaviour
    ///
    /// 1. **Re-read the user by id.** The caller already has one; Go fetches it again, and
    ///    rewrites a miss to 400 and everything else to 500 exactly as `GetUserForLogin` does.
    /// 2. **Preflight**: not deleted, not a bot, not already at the attempt cap — in that order,
    ///    each with its own id. All three run *before* the counter moves, so a deactivated
    ///    account never accumulates failed attempts.
    /// 3. **Claim** a slot, conditionally on being under `MaximumLoginAttempts`.
    /// 4. **Refuse** if the claim failed. The id is the LDAP-flavoured one for an LDAP row and
    ///    the plain one otherwise — but note this is `checkUserLoginAttempts` built *inline*,
    ///    which always uses the plain id regardless of `AuthService`, unlike the preflight check
    ///    in step 2 which branches. Two ids for one condition, reached by two paths.
    /// 5. **Check the password**, refunding the slot for every failure except a credential
    ///    mismatch — so a broken hasher cannot lock anybody out.
    /// 6. **MFA**, refunding only when no token was supplied. Not reached here: the handler
    ///    forwards before step 3 when MFA would do anything, and [`App::check_user_mfa`] is a
    ///    no-op otherwise.
    /// 7. **Zero the counter**, then postflight: e-mail verification.
    ///
    /// Step 7's order matters — the counter is cleared *before* the verification check, so a user
    /// with an unverified e-mail and the right password has their lockout reset even though the
    /// login fails.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, claimed))]
    pub async fn check_password_and_all_criteria(
        &self,
        user_id: &str,
        password: &str,
        mfa_token: &str,
    ) -> AppResult {
        let user = self.get_user(user_id).await.map_err(|mut err| {
            err.status_code = if err.id == "app.user.missing_account.const" {
                400
            } else {
                500
            };
            err
        })?;

        self.check_user_preflight_authentication_criteria(&user)?;

        let max_attempts = clamp_attempts(self.config().maximum_login_attempts);
        let claimed = self
            .store()
            .user()
            .try_increment_failed_password_attempts(&user.id, max_attempts)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "claiming a failed-attempt slot failed");
                attempts_error("CheckPasswordAndAllCriteria")
            })?;
        tracing::Span::current().record("claimed", claimed);

        if !claimed {
            // Built inline at authentication.go:132 and therefore **always** the non-LDAP id,
            // even for an LDAP row — unlike `checkUserLoginAttempts` in the preflight above.
            return Err(AppError::boxed(
                "checkUserLoginAttempts",
                "api.user.check_user_login_attempts.too_many.app_error",
                None,
                format!("user_id={}", user.id),
                401,
            ));
        }

        if let Err(err) = self.check_user_password(&user, password).await {
            if err.id != CHECK_USER_PASSWORD_INVALID
                && let Err(refund) = self
                    .store()
                    .user()
                    .decrement_failed_password_attempts(&user.id)
                    .await
            {
                tracing::warn!(error = %refund, user_id = %user.id, "failed to refund login attempt slot");
            }
            return Err(err);
        }

        if let Err(err) = self.check_user_mfa(&user, mfa_token) {
            if mfa_token.is_empty()
                && let Err(refund) = self
                    .store()
                    .user()
                    .decrement_failed_password_attempts(&user.id)
                    .await
            {
                tracing::warn!(error = %refund, user_id = %user.id, "failed to refund MFA probe slot");
            }
            return Err(err);
        }

        self.store()
            .user()
            .update_failed_password_attempts(&user.id, 0)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "clearing the failed-attempt counter failed");
                attempts_error("CheckPasswordAndAllCriteria")
            })?;

        self.check_user_postflight_authentication_criteria(&user)
    }

    /// Port of `App.CheckUserPreflightAuthenticationCriteria` (authentication.go:373).
    ///
    /// Three checks in a fixed order — deleted, bot, at the cap. The order is observable: a
    /// deactivated bot answers `inactive`, not `bot_login_forbidden`, and a deactivated account
    /// at the cap answers `inactive` rather than `too_many`.
    ///
    /// The MFA token Go passes in is unused by every branch, so it is not a parameter here.
    pub fn check_user_preflight_authentication_criteria(&self, user: &User) -> AppResult {
        check_user_not_disabled(user)?;
        check_user_not_bot(user)?;
        check_user_login_attempts(user, self.config().maximum_login_attempts)
    }

    /// Port of `App.CheckUserPostflightAuthenticationCriteria` (authentication.go:387).
    ///
    /// One check: `!EmailVerified && RequireEmailVerification` is
    /// `api.user.login.not_verified.app_error` at 401 — and it is one of the ids the handler's
    /// mask lets through, so the client is told which of the two it is. Note the config flag is
    /// off on a stock server, so an unverified address logs in fine by default.
    pub fn check_user_postflight_authentication_criteria(&self, user: &User) -> AppResult {
        if !user.email_verified && self.config().require_email_verification {
            return Err(AppError::boxed(
                "Login",
                "api.user.login.not_verified.app_error",
                None,
                format!("user_id={}", user.id),
                401,
            ));
        }
        Ok(())
    }

    /// Port of `App.CheckUserMfa` (authentication.go:395), **first branch only**.
    ///
    /// Go returns `nil` immediately when the user has no MFA enrolled or the server has MFA off,
    /// and only then reaches the token validation this port does not have. `mm_api::login`
    /// forwards any request that would get past that guard, so this can only ever return `Ok` —
    /// the `Err` arm exists so that a *future* caller that forgot to forward fails loudly instead
    /// of authenticating without a second factor.
    ///
    /// The second `if` in Go's source is dead code: it re-tests
    /// `!EnableMultifactorAuthentication`, which the first `if` already returned on.
    pub fn check_user_mfa(&self, user: &User, _token: &str) -> AppResult {
        if !user.mfa_active || !self.config().enable_multifactor_authentication {
            return Ok(());
        }

        tracing::error!(
            user_id = %user.id,
            "MFA validation reached in a port that has none — the caller should have forwarded"
        );
        Err(AppError::boxed(
            "CheckUserMfa",
            "mfa.mfa_disabled.app_error",
            None,
            String::new(),
            501,
        ))
    }

    /// Port of `App.CreateSession` (app/session.go:26).
    ///
    /// Three things happen before the insert, and two of them are easy to miss:
    ///
    /// 1. `limitNumberOfSessions` revokes whatever sits past the 500th most recent session.
    /// 2. The user is re-read and **remote users are refused** — same id as the handler's own
    ///    remote check, and this one guards every caller rather than just `login`. A *missing*
    ///    user is allowed through: Go's comment says the unit tests depend on it.
    /// 3. `PlatformService.CreateSession` blanks `session.Token` before saving, so a caller
    ///    cannot choose its own token; `PreSave` then mints one.
    #[tracing::instrument(skip_all, fields(user_id = %session.user_id, session_id))]
    pub async fn create_session(&self, mut session: Session) -> AppResult<Session> {
        self.limit_number_of_sessions(&session.user_id).await?;

        match self.get_user(&session.user_id).await {
            Ok(user) => {
                if user.is_remote() {
                    return Err(AppError::boxed(
                        "login",
                        "api.user.login.remote_users.login.error",
                        None,
                        String::new(),
                        401,
                    ));
                }
            }
            // `appErr.StatusCode != http.StatusNotFound` — a miss is tolerated, anything else is
            // returned as is.
            Err(err) if err.status_code != 404 => return Err(err),
            Err(_) => {}
        }

        // `session.Token = ""` (platform/session.go:24) — unconditional, before the store's
        // `PreSave` mints one. A caller that set a token has it discarded.
        session.token = String::new();

        let saved = self.store().session().save(session).await.map_err(|err| {
            if err.is_invalid_input() {
                return AppError::boxed(
                    "CreateSession",
                    "app.session.save.existing.app_error",
                    None,
                    String::new(),
                    400,
                );
            }
            tracing::error!(error = %err, "saving a session failed");
            AppError::boxed(
                "CreateSession",
                "app.session.save.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        tracing::Span::current().record("session_id", saved.id.as_str());
        Ok(saved)
    }

    /// Port of `App.limitNumberOfSessions` (app/session.go:155).
    ///
    /// Asks for the 100 sessions sitting past the newest 499 and revokes every one of them. The
    /// offset is `maxSessionsLimit - 1` rather than `maxSessionsLimit` because this runs *before*
    /// the new session is inserted: leaving room for it is the whole point, and an off-by-one
    /// here leaves a user permanently at 501.
    ///
    /// Every failure — the read and each revoke — is `app.session.save.app_error` at 500, which
    /// is why a login can fail with a "save" error before anything is saved.
    async fn limit_number_of_sessions(&self, user_id: &str) -> AppResult {
        let sessions = self
            .store()
            .session()
            .get_lru_sessions(user_id, LIMIT_SESSIONS_RETURN_LIMIT, MAX_SESSIONS_LIMIT - 1)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reading the least recently used sessions failed");
                session_save_error()
            })?;

        for session in &sessions {
            self.revoke_session(session).await.map_err(|err| {
                tracing::error!(error = %err, "revoking a session over the limit failed");
                session_save_error()
            })?;
            tracing::debug!(
                user_id = %user_id,
                session_id = %session.id,
                "session revoked; the user was over maxSessionsLimit"
            );
        }

        Ok(())
    }

    /// Port of `App.DoLogin` (app/login.go:133).
    ///
    /// # The order, and the two writes it leaves behind on failure
    ///
    /// 1. The `UserWillLogIn` plugin hook — no plugin host here, so no rejection is possible.
    /// 2. **Validate both device ids**, each against its own allowlist and with its own 400.
    /// 3. **Force `IsMobile`** when either device id is present. Go's comment: the explicit
    ///    signal beats the user-agent sniff.
    /// 4. Build the session — roles from `GetRawRoles`, the three `isMobile`/`isSaml`/`isOAuthUser`
    ///    props, and a fresh CSRF token.
    /// 5. **Set the expiry** from the mobile or the web length. The SSO length is unreachable
    ///    from this route: `IsSaml` and `IsOAuthUser` are always false here.
    /// 6. **Revoke other sessions holding the same device id**, which is a write — and it happens
    ///    before the session exists, so a failure at step 7 leaves those revocations done.
    /// 7. Add the three user-agent props, then `CreateSession`.
    /// 8. `UpdateLastLogin(user.Id, session.CreateAt)` — the **session's** create time, minted by
    ///    the store, not a fresh clock read. A failure here is a 500 with a session already
    ///    inserted and a token already issued, which is Go's behaviour and is recorded rather
    ///    than corrected: the alternative is deleting a row Go keeps.
    ///
    /// `w.Header().Set(model.HeaderToken, session.Token)` is step 8½ in Go and is the caller's
    /// job here — this function returns the session and `mm_api::login` writes the header.
    ///
    /// The LDAP profile-picture refresh and the `UserHasLoggedIn` hook are both `a.Srv().Go(...)`
    /// background work with no local equivalent.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, is_mobile, session_id))]
    pub async fn do_login(
        &self,
        user: &User,
        opts: &LoginOptions,
        user_agent_header: &str,
    ) -> AppResult<Session> {
        if !opts.device_id.is_empty() && !is_valid_standard_device_id(&opts.device_id) {
            return Err(AppError::boxed(
                "DoLogin",
                "api.user.attach_device_id.invalid_device_id.app_error",
                None,
                String::new(),
                400,
            ));
        }
        if !opts.voip_device_id.is_empty() && !is_valid_voip_device_id(&opts.voip_device_id) {
            return Err(AppError::boxed(
                "DoLogin",
                "api.user.attach_device_id.invalid_voip_device_id.app_error",
                None,
                String::new(),
                400,
            ));
        }

        // "Presence of a device or VoIP token is authoritative for mobile-ness" (app/login.go:154).
        let is_mobile =
            opts.is_mobile || !opts.device_id.is_empty() || !opts.voip_device_id.is_empty();
        tracing::Span::current().record("is_mobile", is_mobile);

        let mut props = StringMap::new();
        props.insert(
            mm_model::session::external::USER_AUTH_SERVICE_IS_MOBILE.to_owned(),
            is_mobile.to_string(),
        );
        props.insert(
            mm_model::session::external::USER_AUTH_SERVICE_IS_SAML.to_owned(),
            opts.is_saml.to_string(),
        );
        props.insert(
            mm_model::session::external::USER_AUTH_SERVICE_IS_OAUTH.to_owned(),
            opts.is_oauth_user.to_string(),
        );

        let mut session = Session {
            user_id: user.id.clone(),
            roles: user.get_raw_roles().to_owned(),
            device_id: opts.device_id.clone(),
            voip_device_id: opts.voip_device_id.clone(),
            is_oauth: false,
            props: Some(props),
            ..Session::default()
        };
        session.generate_csrf();

        let hours = if is_mobile {
            self.config().session_length_mobile_in_hours
        } else {
            // Go's middle arm — `IsOAuthUser || IsSaml` and `SessionLengthSSOInHours` — is
            // unreachable from `login`, which never sets either flag. Not modelled; see the
            // module doc.
            self.config().session_length_web_in_hours
        };
        self.set_session_expire_in_hours(&mut session, hours);

        if !opts.device_id.is_empty() {
            self.revoke_other_sessions_for_device_id(&user.id, &opts.device_id, "", false)
                .await
                .map_err(|mut err| {
                    err.status_code = 500;
                    err
                })?;
        }
        if !opts.voip_device_id.is_empty() {
            self.revoke_other_sessions_for_device_id(&user.id, &opts.voip_device_id, "", true)
                .await
                .map_err(|mut err| {
                    err.status_code = 500;
                    err
                })?;
        }

        let agent = user_agent::session_props(user_agent_header);
        session.add_prop(SESSION_PROP_PLATFORM, agent.platform);
        session.add_prop(SESSION_PROP_OS, agent.os);
        session.add_prop(SESSION_PROP_BROWSER, agent.browser);
        session.add_prop(
            SESSION_PROP_IS_GUEST,
            if user.is_guest() { "true" } else { "false" },
        );

        let session = self.create_session(session).await.map_err(|mut err| {
            err.status_code = 500;
            err
        })?;
        tracing::Span::current().record("session_id", session.id.as_str());

        self.store()
            .user()
            .update_last_login(&user.id, session.create_at)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "updating LastLogin failed");
                AppError::boxed(
                    "DoLogin",
                    "app.login.doLogin.updateLastLogin.error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(session)
    }
}

/// `model.NewAppError("limitNumberOfSessions", "app.session.save.app_error", …, 500)`.
fn session_save_error() -> Box<AppError> {
    AppError::boxed(
        "limitNumberOfSessions",
        "app.session.save.app_error",
        None,
        String::new(),
        500,
    )
}

/// Port of `checkUserLoginAttempts` (authentication.go:490).
///
/// **An LDAP row gets a different id** — `…too_many_ldap.app_error` — and both are on the
/// handler's unmasked list, so the client is told which. The comparison is `>=`, matching the
/// strict `<` in the store's claim: `maxAttempts` failures land the counter on `maxAttempts` and
/// the next attempt is refused here before the counter is touched again.
fn check_user_login_attempts(user: &User, max_attempts: i64) -> AppResult {
    if user.failed_attempts >= max_attempts {
        let id = if user.auth_service == mm_model::user::external::USER_AUTH_SERVICE_LDAP {
            "api.user.check_user_login_attempts.too_many_ldap.app_error"
        } else {
            "api.user.check_user_login_attempts.too_many.app_error"
        };
        return Err(AppError::boxed(
            "checkUserLoginAttempts",
            id,
            None,
            format!("user_id={}", user.id),
            401,
        ));
    }
    Ok(())
}

/// Port of `checkUserNotDisabled` (authentication.go:502). `DeleteAt > 0`, not `!= 0`.
fn check_user_not_disabled(user: &User) -> AppResult {
    if user.delete_at > 0 {
        return Err(AppError::boxed(
            "Login",
            "api.user.login.inactive.app_error",
            None,
            format!("user_id={}", user.id),
            401,
        ));
    }
    Ok(())
}

/// Port of `checkUserNotBot` (authentication.go:509).
///
/// `user.IsBot` is the `Bots` join, not a role — so a bot's personal access token still works
/// while its password never does, which is the point of the check.
fn check_user_not_bot(user: &User) -> AppResult {
    if user.is_bot {
        return Err(AppError::boxed(
            "Login",
            "api.user.login.bot_login_forbidden.app_error",
            None,
            format!("user_id={}", user.id),
            401,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn user() -> User {
        User {
            id: "loginuserloginuserloginus1".to_owned(),
            username: "loginuser".to_owned(),
            email: "login@example.com".to_owned(),
            ..User::default()
        }
    }

    /// The preflight's three checks run in a fixed order, and the order decides the id.
    ///
    /// A deactivated bot is `inactive`, not `bot_login_forbidden`; a deactivated account at the
    /// cap is `inactive`, not `too_many`; a bot at the cap is `bot_login_forbidden`. A port that
    /// ran the cheapest check first, or that collected all three, would answer differently on
    /// every one of these — and each id is on the handler's **unmasked** list, so the difference
    /// reaches the client.
    #[tokio::test]
    async fn the_preflight_order_decides_which_refusal_the_client_sees() {
        let app = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            Config {
                maximum_login_attempts: 10,
                ..Config::default()
            },
        );

        let cases: &[(&str, User)] = &[
            (
                "api.user.login.inactive.app_error",
                User {
                    delete_at: 1,
                    is_bot: true,
                    failed_attempts: 99,
                    ..user()
                },
            ),
            (
                "api.user.login.bot_login_forbidden.app_error",
                User {
                    is_bot: true,
                    failed_attempts: 99,
                    ..user()
                },
            ),
            (
                "api.user.check_user_login_attempts.too_many.app_error",
                User {
                    failed_attempts: 10,
                    ..user()
                },
            ),
        ];

        for (want, subject) in cases {
            let err = app
                .check_user_preflight_authentication_criteria(subject)
                .expect_err("refused");
            assert_eq!(&err.id, want);
            assert_eq!(err.status_code, 401);
        }

        // And one that passes all three: exactly one below the cap.
        assert!(
            app.check_user_preflight_authentication_criteria(&User {
                failed_attempts: 9,
                ..user()
            })
            .is_ok(),
            "nine of ten attempts still logs in"
        );
    }

    /// `DeleteAt > 0`, so a **negative** `DeleteAt` is not deactivated. Unreachable through the
    /// API, reachable by a bad import, and the two readings differ.
    #[test]
    fn deactivation_is_strictly_positive() {
        assert!(
            check_user_not_disabled(&User {
                delete_at: -1,
                ..user()
            })
            .is_ok()
        );
        assert!(
            check_user_not_disabled(&User {
                delete_at: 0,
                ..user()
            })
            .is_ok()
        );
        assert!(
            check_user_not_disabled(&User {
                delete_at: 1,
                ..user()
            })
            .is_err()
        );
    }

    /// The lockout id **differs for an LDAP row**, and both ids are unmasked — so this is the one
    /// place `AuthService` is visible to an unauthenticated client.
    #[test]
    fn the_lockout_id_names_ldap_separately() {
        let at_cap = User {
            failed_attempts: 10,
            ..user()
        };
        assert_eq!(
            check_user_login_attempts(&at_cap, 10)
                .expect_err("at the cap")
                .id,
            "api.user.check_user_login_attempts.too_many.app_error"
        );

        let ldap = User {
            failed_attempts: 10,
            auth_service: "ldap".to_owned(),
            ..user()
        };
        assert_eq!(
            check_user_login_attempts(&ldap, 10)
                .expect_err("at the cap")
                .id,
            "api.user.check_user_login_attempts.too_many_ldap.app_error"
        );

        // The boundary: `>=`, so the tenth failure of ten is already a refusal.
        assert!(
            check_user_login_attempts(
                &User {
                    failed_attempts: 9,
                    ..user()
                },
                10
            )
            .is_ok()
        );
    }

    /// `check_user_mfa` is a pass-through **only** while the pair that would need real MFA is
    /// false. Both halves are asserted, because a port that returned `Ok` unconditionally would
    /// authenticate an MFA user with no second factor and no test would notice.
    #[tokio::test]
    async fn check_user_mfa_passes_only_when_mfa_cannot_apply() {
        let off = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            Config {
                enable_multifactor_authentication: false,
                ..Config::default()
            },
        );
        let on = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            Config {
                enable_multifactor_authentication: true,
                ..Config::default()
            },
        );
        let enrolled = User {
            mfa_active: true,
            ..user()
        };

        assert!(off.check_user_mfa(&user(), "").is_ok(), "nobody enrolled");
        assert!(
            off.check_user_mfa(&enrolled, "").is_ok(),
            "server has it off"
        );
        assert!(on.check_user_mfa(&user(), "").is_ok(), "user has it off");
        assert!(
            on.check_user_mfa(&enrolled, "123456").is_err(),
            "both on must refuse rather than silently accept"
        );
    }

    /// The postflight reads **both** its inputs. Neither alone refuses.
    #[tokio::test]
    async fn the_email_verification_refusal_needs_both_halves() {
        let required = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            Config {
                require_email_verification: true,
                ..Config::default()
            },
        );
        let relaxed = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://x/y")
                    .expect("a lazy pool needs no server"),
            ),
            Config::default(),
        );
        let verified = User {
            email_verified: true,
            ..user()
        };

        let err = required
            .check_user_postflight_authentication_criteria(&user())
            .expect_err("unverified and required");
        assert_eq!(err.id, "api.user.login.not_verified.app_error");
        assert_eq!(err.status_code, 401);

        assert!(
            required
                .check_user_postflight_authentication_criteria(&verified)
                .is_ok()
        );
        assert!(
            relaxed
                .check_user_postflight_authentication_criteria(&user())
                .is_ok(),
            "the stock server does not require verification"
        );
    }
}
