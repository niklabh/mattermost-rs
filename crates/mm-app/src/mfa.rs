//! Port of `App.MFARequired` (app/authentication.go:378).
//!
//! The one caller today is the websocket's `IsMFAAuthenticated` (web_conn.go:811), through
//! [`App::conn_is_authenticated`](crate::App::conn_is_authenticated). REST's `RequireMfa` is not
//! wired yet ([D-801]).
//!
//! # Three gates, then the user
//!
//! Nothing is looked up unless the licence carries the MFA feature **and** MFA is enabled **and**
//! enforced. Past that, the session must exist, OAuth sessions are exempt, and then the user is
//! read: guests are exempt unless guest enforcement is on, accounts on a service other than email
//! or LDAP are exempt, a request for `/api/v4/users/me` is exempt, bots are exempt, and anyone
//! else without `MfaActive` is refused.

use mm_model::license::License;
use mm_model::session::Session;
use mm_model::user::external::USER_AUTH_SERVICE_LDAP;
use mm_model::user::{USER_AUTH_SERVICE_EMAIL, User};
use mm_model::utils::{AppError, AppResult};

use crate::App;

impl App {
    /// Port of `App.MFARequired` (app/authentication.go:378).
    ///
    /// `is_users_me_path` is Go's `rctx.Path() == path.Join(subpath, "/api/v4/users/me")`; a
    /// caller with no request path — the websocket — passes `false`.
    ///
    /// The licence is read through [`App::license`]; a licence that cannot be read counts as no
    /// licence, which closes the first gate, as Go's nil `License()` does.
    pub async fn mfa_required(
        &self,
        session: Option<&Session>,
        is_users_me_path: bool,
    ) -> AppResult<()> {
        let licensed_mfa = match self.license().await {
            Ok(Some(license)) => license_has_mfa(&license),
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(error = %err, "MfaRequired: the licence could not be read");
                false
            }
        };
        let config = self.config();
        if !mfa_gate_open(
            licensed_mfa,
            config.enable_multifactor_authentication,
            config.enforce_multifactor_authentication,
        ) {
            return Ok(());
        }

        // "Session cannot be nil or empty if MFA is to be enforced."
        let Some(session) = session.filter(|session| !session.id.is_empty()) else {
            return Err(AppError::boxed(
                "MfaRequired",
                "api.context.get_session.app_error",
                None,
                String::new(),
                401,
            ));
        };

        // "OAuth integrations are excepted"
        if session.is_oauth {
            return Ok(());
        }

        let user = self.get_user(&session.user_id).await.map_err(|err| {
            tracing::debug!(error = %err, user_id = %session.user_id, "MfaRequired: user lookup failed");
            AppError::boxed(
                "MfaRequired",
                "api.context.get_user.app_error",
                None,
                err.to_string(),
                500,
            )
        })?;

        if user_owes_mfa(
            &user,
            config.guest_accounts_enforce_multifactor_authentication,
            is_users_me_path,
        ) {
            return Err(AppError::boxed(
                "MfaRequired",
                "api.context.mfa_required.app_error",
                None,
                String::new(),
                403,
            ));
        }
        Ok(())
    }
}

/// `*license.Features.MFA` after `SetDefaults`, which resolves an absent flag to `FutureFeatures`
/// — itself `true` when absent (license.go:250).
fn license_has_mfa(license: &License) -> bool {
    let mut features = license.features.clone().unwrap_or_default();
    features.set_defaults();
    features.mfa.unwrap_or(false)
}

/// The first `if` of `MFARequired`: all three must hold for anything to be checked.
fn mfa_gate_open(licensed_mfa: bool, enabled: bool, enforced: bool) -> bool {
    licensed_mfa && enabled && enforced
}

/// The user half of `MFARequired`, past the session checks: `true` when this user is refused.
fn user_owes_mfa(user: &User, guest_enforcement: bool, is_users_me_path: bool) -> bool {
    if user.is_guest() && !guest_enforcement {
        return false;
    }
    // "Only required for email and ldap accounts"
    if !user.auth_service.is_empty()
        && user.auth_service != USER_AUTH_SERVICE_EMAIL
        && user.auth_service != USER_AUTH_SERVICE_LDAP
    {
        return false;
    }
    // "Special case to let user get themself"
    if is_users_me_path {
        return false;
    }
    // "Bots are exempt"
    if user.is_bot {
        return false;
    }
    !user.mfa_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::license::Features;

    #[test]
    fn nothing_is_checked_unless_all_three_gates_hold() {
        for licensed in [false, true] {
            for enabled in [false, true] {
                for enforced in [false, true] {
                    assert_eq!(
                        mfa_gate_open(licensed, enabled, enforced),
                        licensed && enabled && enforced,
                        "{licensed} {enabled} {enforced}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_absent_mfa_feature_follows_future_features() {
        let mut license = License::default();
        assert!(
            license_has_mfa(&license),
            "no features at all: FutureFeatures defaults true"
        );
        license.features = Some(Features {
            future_features: Some(false),
            ..Features::default()
        });
        assert!(
            !license_has_mfa(&license),
            "absent MFA takes FutureFeatures"
        );
        license.features = Some(Features {
            future_features: Some(true),
            mfa: Some(false),
            ..Features::default()
        });
        assert!(!license_has_mfa(&license), "an explicit flag wins");
    }

    fn user() -> User {
        User {
            id: "uuuuuuuuuuuuuuuuuuuuuuuuuu".to_owned(),
            roles: "system_user".to_owned(),
            ..User::default()
        }
    }

    #[test]
    fn a_plain_email_user_without_mfa_owes_it_and_with_mfa_does_not() {
        let mut u = user();
        assert!(user_owes_mfa(&u, false, false));
        u.auth_service = USER_AUTH_SERVICE_EMAIL.to_owned();
        assert!(user_owes_mfa(&u, false, false));
        u.auth_service = USER_AUTH_SERVICE_LDAP.to_owned();
        assert!(user_owes_mfa(&u, false, false));
        u.mfa_active = true;
        assert!(!user_owes_mfa(&u, false, false));
    }

    #[test]
    fn each_exemption_exempts_on_its_own() {
        let mut guest = user();
        guest.roles = "system_guest".to_owned();
        assert!(
            !user_owes_mfa(&guest, false, false),
            "guest, enforcement off"
        );
        assert!(user_owes_mfa(&guest, true, false), "guest, enforcement on");

        for service in ["gitlab", "saml", "openid"] {
            let mut sso = user();
            sso.auth_service = service.to_owned();
            assert!(!user_owes_mfa(&sso, false, false), "{service}");
        }

        assert!(!user_owes_mfa(&user(), false, true), "/users/me");

        let mut bot = user();
        bot.is_bot = true;
        assert!(!user_owes_mfa(&bot, false, false), "bot");
    }
}
