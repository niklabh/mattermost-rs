//! The authentication-data half of the user vertical: `App.UpdateUserAuth` (app/user.go:1488)
//! and the two MFA entry points behind `PUT /users/{id}/mfa` and `POST /users/{id}/mfa/generate`.
//!
//! # `UpdateUserAuth` is the most destructive operation on an account short of deleting it
//!
//! Three things happen and only the first is named in the function: the store blanks `Password`
//! along with writing the two auth columns, every session the account has is revoked, and the
//! account's e-mail address is left alone. An account moved to `gitlab` therefore cannot log in
//! with the password it had a moment ago, and is not logged in anywhere either. That is the
//! intended behaviour — an SSO account with a live local password is a back door — but it means a
//! caller who gets the `auth_service` wrong has locked the account out, and nothing in the route
//! confirms anything.
//!
//! # MFA
//!
//! [`App::generate_mfa_secret`], [`App::update_mfa`] and the activate/deactivate pair behind it
//! port `App` and `platform/shared/mfa` whole; the TOTP and the QR code are
//! [`crate::otp`], checked against Go's own `dgoogauth` and `rsc/qr` output. The flag is the
//! second check in both generate and activate — after `GetUser`, and in activation after the
//! auth-service refusal too — and deactivation has none at all.

use mm_model::mfa_secret::MfaSecret;
use mm_model::user::external::USER_AUTH_SERVICE_LDAP;
use mm_model::user::{USER_AUTH_SERVICE_EMAIL, UserAuth};
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, UserStore};

use crate::App;

impl App {
    /// Port of `App.UpdateUserAuth` (app/user.go:1488).
    ///
    /// Four steps, of which Go names one:
    ///
    /// 1. [`mm_store::UserStore::update_auth_data`] — the two auth columns **plus** a blanked
    ///    `Password`, a zeroed `FailedAttempts` and a bumped `UpdateAt`/`LastPasswordUpdate`.
    /// 2. `InvalidateCacheForUser` — an in-process cache this server does not have. The *Go*
    ///    process does, and it is still running beside this one, so a profile it has cached keeps
    ///    the old `AuthService` until its own invalidation fires. See [D-085].
    /// 3. `RevokeAllSessions` — every session, including the caller's own if an administrator
    ///    points this at themselves.
    /// 4. The submitted `UserAuth` is returned **unchanged**; nothing is re-read. So the response
    ///    describes what was asked for, not what is stored, and the two differ whenever the id
    ///    matched no row — which is a 200. See the store doc.
    ///
    /// The two error ids differ only by which failure the store reported: an `ErrInvalidInput`
    /// (a unique violation, almost always on `AuthData`) is
    /// `app.user.update_auth_data.email_exists.app_error` at 400, and everything else is
    /// `app.user.update_auth_data.app_error` at 500.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, auth_service = %user_auth.auth_service))]
    pub async fn update_user_auth(
        &self,
        user_id: &str,
        user_auth: &UserAuth,
    ) -> AppResult<UserAuth> {
        self.store()
            .user()
            .update_auth_data(
                user_id,
                &user_auth.auth_service,
                user_auth.auth_data.as_deref(),
            )
            .await
            .map_err(|err| {
                // `errors.As(err, &invErr)` — the unique violation, and nothing else, is the 400.
                if matches!(err, StoreError::InvalidInput { .. }) {
                    AppError::boxed(
                        "UpdateUserAuth",
                        "app.user.update_auth_data.email_exists.app_error",
                        None,
                        String::new(),
                        400,
                    )
                } else {
                    tracing::error!(error = %err, "updating auth data failed");
                    AppError::boxed(
                        "UpdateUserAuth",
                        "app.user.update_auth_data.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        self.revoke_all_sessions(user_id).await?;

        Ok(user_auth.clone())
    }

    /// Port of `App.GenerateMfaSecret` (app/user.go:937) and `UserService.GenerateMfaSecret`
    /// (users/users.go:249).
    ///
    /// `GetUser` first — so an unknown id is a **404 before** the disabled-MFA 501, and a caller
    /// cannot use this route to probe whether MFA is on without naming a real account. Then 20
    /// bytes of randomness, the `otpauth://` link for `ServiceSettings.SiteURL` and the user's
    /// e-mail, the QR code's PNG ([`crate::otp::generate_secret`]), and the write: `MfaSecret`
    /// replaced, the replay list emptied, `UpdateAt` bumped. `MfaActive` is untouched, so a user
    /// who generates a new secret while enrolled stays enrolled — on the new secret, which their
    /// authenticator does not have yet. Any failure past the flag is the one 500,
    /// `mfa.generate_qr_code.create_code.app_error`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn generate_mfa_secret(&self, user_id: &str) -> AppResult<MfaSecret> {
        let user = self.get_user(user_id).await?;

        if !self.config().enable_multifactor_authentication {
            return Err(mfa_disabled("GenerateMfaSecret"));
        }

        let create_code_failed = || {
            AppError::boxed(
                "GenerateMfaSecret",
                "mfa.generate_qr_code.create_code.app_error",
                None,
                String::new(),
                500,
            )
        };
        let site_url = self.config().site_url.clone().unwrap_or_default();
        let generated = crate::otp::generate_secret(&site_url, &user.email).map_err(|err| {
            tracing::error!(error = %err, "rendering the MFA QR code failed");
            create_code_failed()
        })?;
        self.store()
            .user()
            .update_mfa_secret(&user.id, &generated.secret)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "storing the MFA secret failed");
                create_code_failed()
            })?;
        self.invalidate_cache_for_user(&user.id).await;

        Ok(MfaSecret {
            secret: generated.secret,
            qr_code: crate::otp::qr_code_base64(&generated.png),
        })
    }

    /// Port of `App.ActivateMfa` (app/user.go:955) over `mfa.Activate`.
    ///
    /// The order is `GetUser`, then the auth-service refusal, then the flag — so an `ldap` or
    /// email account on a server with MFA off gets the 501, and a `gitlab` account gets a **400**
    /// instead, never learning that MFA is disabled. The refusal's predicate is
    /// `AuthService != "" && AuthService != "ldap"`, so the empty string — an ordinary
    /// password account — passes, and `email` spelled out explicitly does **not**: nothing on
    /// this route normalises the two, and `updateUserAuth` is the reason a row can hold either.
    ///
    /// Past the flag, the token is checked against the stored secret at the current step
    /// ([`crate::otp::authenticate`]). A well-formed code that does not match is the **401**
    /// `mfa.activate.bad_token.app_error`; everything else that fails — a code that is not six
    /// digits among them, since `errors.Is(err, mfa.InvalidToken)` is false for dgoogauth's parse
    /// error — is the **500** `mfa.activate.app_error`. On success `MfaActive` is set and then the
    /// replay list stored, two writes, in that order.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn activate_mfa(&self, user_id: &str, token: &str) -> AppResult<()> {
        let user = self.get_user(user_id).await?;

        if !user.auth_service.is_empty() && user.auth_service != USER_AUTH_SERVICE_LDAP {
            return Err(AppError::boxed(
                "ActivateMfa",
                "api.user.activate_mfa.email_and_ldap_only.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if !self.config().enable_multifactor_authentication {
            return Err(mfa_disabled("ActivateMfa"));
        }

        let activate_failed = || {
            AppError::boxed(
                "ActivateMfa",
                "mfa.activate.app_error",
                None,
                String::new(),
                500,
            )
        };
        let users = self.store().user();
        let used = users
            .get_mfa_used_timestamps(&user.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reading the MFA replay list failed");
                activate_failed()
            })?;
        let secret = user.mfa_secret.as_str();
        let reuse = match crate::otp::authenticate(secret, &used, token, crate::otp::current_step())
        {
            Ok(reuse) => reuse,
            Err(crate::otp::TokenError::Invalid) => {
                return Err(AppError::boxed(
                    "ActivateMfa",
                    "mfa.activate.bad_token.app_error",
                    None,
                    String::new(),
                    401,
                ));
            }
            Err(crate::otp::TokenError::Parse) => return Err(activate_failed()),
        };
        users
            .update_mfa_active(&user.id, true)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "setting MfaActive failed");
                activate_failed()
            })?;
        users
            .store_mfa_used_timestamps(&user.id, &reuse)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "storing the MFA replay list failed");
                activate_failed()
            })?;
        self.invalidate_cache_for_user(&user.id).await;
        Ok(())
    }

    /// Port of `App.DeactivateMfa` (app/user.go:984) over `mfa.Deactivate`: `GetUser`, then
    /// `MfaActive = false`, then `MfaSecret = ''` (which also empties the replay list). **No
    /// configuration gate**: a user can switch MFA off on a server that has it disabled. Either
    /// write failing is the 500 `mfa.deactivate.app_error`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn deactivate_mfa(&self, user_id: &str) -> AppResult<()> {
        let user = self.get_user(user_id).await?;
        let deactivate_failed = |err: StoreError| {
            tracing::error!(error = %err, "deactivating MFA failed");
            AppError::boxed(
                "DeactivateMfa",
                "mfa.deactivate.app_error",
                None,
                String::new(),
                500,
            )
        };
        let users = self.store().user();
        users
            .update_mfa_active(&user.id, false)
            .await
            .map_err(deactivate_failed)?;
        users
            .update_mfa_secret(&user.id, "")
            .await
            .map_err(deactivate_failed)?;
        self.invalidate_cache_for_user(&user.id).await;
        Ok(())
    }

    /// Port of `App.UpdateMfa` (app/user.go:1723): activate or deactivate, then the MFA-change
    /// e-mail from a goroutine — re-reading the user, and only logging a failure, so the e-mail is
    /// never part of the answer.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, activate))]
    pub async fn update_mfa(&self, activate: bool, user_id: &str, token: &str) -> AppResult<()> {
        if activate {
            self.activate_mfa(user_id, token).await?;
        } else {
            self.deactivate_mfa(user_id).await?;
        }
        let app = self.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            let user = match app.get_user(&user_id).await {
                Ok(user) => user,
                Err(err) => {
                    tracing::error!(error = %err.id, "Failed to get user");
                    return;
                }
            };
            let site_url = app.live_site_url().await.unwrap_or_default();
            if let Err(err) = app
                .send_mfa_change_email(&user.email, activate, &user.locale, &site_url)
                .await
            {
                tracing::error!(error = %err, "Failed to send mfa change email");
            }
        });
        Ok(())
    }
}

/// `model.NewAppError(where, "mfa.mfa_disabled.app_error", nil, "", http.StatusNotImplemented)`.
///
/// **501, not 400 and not 403.** Two call sites spell the same error with different `where`
/// values; `where` is not on the wire, so the distinction is for the log alone.
fn mfa_disabled(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "mfa.mfa_disabled.app_error",
        None,
        String::new(),
        501,
    )
}

/// `userAuth.AuthService == model.UserAuthServiceEmail` → `userAuth.AuthService = ""`
/// (api4/user.go:1886).
///
/// The one line of `updateUserAuth` that changes the request before it is applied, and the reason
/// `PUT /users/{id}/auth` with `{"auth_service":"email"}` answers `{}` rather than
/// `{"auth_service":"email"}`: the field is blanked, and `auth_service` carries `omitempty`. It
/// runs **after** [`UserAuth::is_valid`], which is what forces `auth_data` to be absent on that
/// branch — so the stored row ends up with `AuthService = ''` and `AuthData = NULL`, which is
/// exactly what a freshly created password account looks like.
///
/// Lives here rather than in the handler because it is the boundary between what a client asked
/// for and what is written, and a reader changing either needs to see it.
#[must_use]
pub fn normalise_email_auth_service(user_auth: &UserAuth) -> UserAuth {
    let mut normalised = user_auth.clone();
    if normalised.auth_service == USER_AUTH_SERVICE_EMAIL {
        normalised.auth_service.clear();
    }
    normalised
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `email` is the only service that is blanked, and blanking it is not the same as rejecting
    /// it: the request succeeds and writes `AuthService = ''`.
    #[test]
    fn only_email_is_blanked_and_auth_data_is_carried_through() {
        let email = UserAuth {
            auth_data: None,
            auth_service: "email".to_owned(),
        };
        assert_eq!(normalise_email_auth_service(&email).auth_service, "");

        for service in ["ldap", "saml", "gitlab", "google", "office365", "openid"] {
            let other = UserAuth {
                auth_data: Some("ad".to_owned()),
                auth_service: service.to_owned(),
            };
            let normalised = normalise_email_auth_service(&other);
            assert_eq!(normalised.auth_service, service, "{service} is untouched");
            assert_eq!(
                normalised.auth_data.as_deref(),
                Some("ad"),
                "{service} keeps its auth data"
            );
        }
    }

    /// Case matters: Go compares against the constant with `==`, so `Email` is not `email` and
    /// would fail `IsValid` one line earlier anyway. Asserted so that a future
    /// `eq_ignore_ascii_case` "fix" fails here rather than silently widening the route.
    #[test]
    fn the_comparison_is_case_sensitive() {
        let shouting = UserAuth {
            auth_data: None,
            auth_service: "Email".to_owned(),
        };
        assert_eq!(
            normalise_email_auth_service(&shouting).auth_service,
            "Email"
        );
    }

    /// The 501 is spelled the same from both call sites and differs only in `where`, which is not
    /// on the wire. A mutation that returned 400 or 403 from either is caught here as well as by
    /// the parity suite.
    #[test]
    fn the_mfa_disabled_error_is_a_501() {
        for site in ["GenerateMfaSecret", "ActivateMfa"] {
            let err = mfa_disabled(site);
            assert_eq!(err.id, "mfa.mfa_disabled.app_error");
            assert_eq!(err.status_code, 501);
            assert_eq!(err.where_, site);
        }
    }
}
