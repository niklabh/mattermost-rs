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
//! # MFA: every path that would touch a secret is a gate on this deployment
//!
//! `ServiceSettings.EnableMultifactorAuthentication` is off, and it is the *second* check in both
//! [`App::generate_mfa_secret`] and [`App::activate_mfa`] — after `GetUser`, and in `ActivateMfa`
//! after the auth-service refusal too. So the reachable answers are a 404, a 400 and a 501, all
//! three decided from a `SELECT` and the configuration, and the TOTP library Go reaches for
//! afterwards is never entered. Neither function ports that library: a secret generated here
//! could not be compared against Go's, which is generated from `crypto/rand`, and a token
//! validated here could not be shown to agree with `dgoogauth`. The api layer forwards the whole
//! request when the flag is on; the `Err` arms below exist so that a caller which forgot to
//! forward fails loudly instead of quietly authenticating without a second factor — the same
//! shape as [`crate::login`]'s [`App::check_user_mfa`].
//!
//! Deactivation is *not* here. `DeactivateMfa` has no configuration gate at all, writes
//! `MfaActive = false` and `MfaSecret = ''`, and then sends an MFA-change e-mail from a
//! goroutine — a side effect after the write, which is the shape this process forwards rather
//! than diverges on ([D-238]). See [`crate::App::get_user`] and the api module for where that
//! decision is taken.

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

    /// Port of `App.GenerateMfaSecret` (app/user.go:937), as far as this deployment can reach.
    ///
    /// `GetUser` first — so an unknown id is a **404 before** the disabled-MFA 501, and a caller
    /// cannot use this route to probe whether MFA is on without naming a real account. Measured
    /// in that order against the running server.
    ///
    /// Past the flag, Go mints 160 bits from `crypto/rand`, renders a QR PNG, and **writes**
    /// `MfaSecret`. None of that is here; the api layer forwards the request before this is
    /// called when the flag is on.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn generate_mfa_secret(&self, user_id: &str) -> AppResult<MfaSecret> {
        let _user = self.get_user(user_id).await?;

        if !self.config().enable_multifactor_authentication {
            return Err(mfa_disabled("GenerateMfaSecret"));
        }

        tracing::error!(
            user_id = %user_id,
            "MFA secret generation reached in a port that has none — the caller should have forwarded"
        );
        Err(mfa_disabled("GenerateMfaSecret"))
    }

    /// Port of `App.ActivateMfa` (app/user.go:955), as far as this deployment can reach.
    ///
    /// The order is `GetUser`, then the auth-service refusal, then the flag — so an `ldap` or
    /// email account on a server with MFA off gets the 501, and a `gitlab` account gets a **400**
    /// instead, never learning that MFA is disabled. The refusal's predicate is
    /// `AuthService != "" && AuthService != "ldap"`, so the empty string — an ordinary
    /// password account — passes, and `email` spelled out explicitly does **not**: nothing on
    /// this route normalises the two, and `updateUserAuth` is the reason a row can hold either.
    ///
    /// The token is not validated here and the parameter is unused; Go hands it to `dgoogauth`
    /// past the flag. See the module doc.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn activate_mfa(&self, user_id: &str, _token: &str) -> AppResult<()> {
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

        tracing::error!(
            user_id = %user_id,
            "MFA activation reached in a port that has none — the caller should have forwarded"
        );
        Err(mfa_disabled("ActivateMfa"))
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
