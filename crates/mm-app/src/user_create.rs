//! Account creation — the app half of `POST /api/v4/users`.
//!
//! Go spreads this across three files and the shapes do not survive being read one at a time, so
//! the whole chain is ported here in Go's order:
//!
//! ```text
//! App.CreateUserFromSignup   (app/user.go:286)  the anonymous signup
//! App.CreateUserAsAdmin      (app/user.go:273)  a system admin creating somebody
//! App.IsUserSignUpAllowed    (app/user.go:310)
//! App.CreateUser             (app/user.go:324)  -> createUserOrGuest(..., guest=false)
//! users.UserService.CreateUser (app/users/users.go:26)
//! users.UserService.createUser (app/users/users.go:63)
//! ```
//!
//! # What is *not* here, and why that is not a gap in this file
//!
//! `CreateUserWithToken` and `CreateUserWithInviteId` are the two branches the api layer forwards
//! (see `mm_api::user_creates`). Both end in `JoinUserToTeam` plus `AddDirectChannels`, neither of
//! which is ported, and both are selected by a **query parameter** — so the forward is decided
//! before this module is entered and before any row is written. `CreateGuest` is only reachable
//! from the token branch, so `guest = true` is not modelled at all rather than modelled and left
//! untested.
//!
//! # The e-mail that is not sent
//!
//! Every one of Go's four branches ends with `SendWelcomeEmail`, whose failure is a
//! `Logger.Warn` and nothing else — it cannot change the response. There is no e-mail service in
//! this port ([D-238]), so the served branches skip it and log. That is a real divergence on a
//! server with SMTP configured and it is recorded as [D-450]; it is *not* a reason to forward,
//! because the send happens strictly after the user row is committed and a forward there would
//! create the account twice.

use mm_model::preference::{
    PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS, PREFERENCE_CATEGORY_SYSTEM_NOTICE,
    PREFERENCE_CATEGORY_TUTORIAL_STEPS, PREFERENCE_NAME_RECOMMENDED_NEXT_STEPS_HIDE, Preference,
    Preferences,
};
use mm_model::role::{SYSTEM_ADMIN_ROLE_ID, SYSTEM_USER_ROLE_ID};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::error::StoreError;
use mm_store::{PreferenceStore, UserStore};

use crate::App;

impl App {
    /// Port of `App.IsUserSignUpAllowed` (app/user.go:310).
    ///
    /// **One id for two switches.** `EnableSignUpWithEmail` and `EnableUserCreation` are OR-ed
    /// into the same `api.user.create_user.signup_email_disabled.app_error` at **501**, so a
    /// client cannot tell which of them is off. The status is `NotImplemented`, not `Forbidden` —
    /// a detail worth keeping, because the neighbouring open-server refusal is a 403 and the two
    /// are otherwise easy to confuse.
    pub fn is_user_signup_allowed(&self) -> AppResult {
        let config = self.config();
        if !config.enable_sign_up_with_email || !config.enable_user_creation {
            return Err(AppError::boxed(
                "IsUserSignUpAllowed",
                "api.user.create_user.signup_email_disabled.app_error",
                None,
                String::new(),
                501,
            ));
        }
        Ok(())
    }

    /// Port of `App.IsFirstUserAccount` (app/user.go:318) via
    /// `platform.PlatformService.IsFirstUserAccount`.
    ///
    /// # Go caches this and the cache is a one-way latch
    ///
    /// `platform.IsFirstUserAccount` keeps an `atomic.Bool` that starts true and is set to false
    /// the first time the count comes back non-zero, after which the store is never consulted
    /// again. This port asks the store every time. The two agree on every reachable sequence —
    /// the latch only ever suppresses a *second* "yes", and a second yes cannot happen because
    /// the first signup inserts a row — but on a server whose only user is then permanently
    /// deleted, Go keeps saying "not first" while this says "first". That is unreachable through
    /// the REST API, which has no permanent-delete-the-last-user route.
    ///
    /// A store failure is **swallowed into `false`**: Go's `IsFirstUserAccount` returns a plain
    /// `bool` and logs, so a database blip makes the server behave as though it already has
    /// users, which is the safe direction.
    ///
    /// # `IncludeDeleted: true`, and it is not the default
    ///
    /// This is the *only* `UserCountOptions` in the tree that sets it, and it is the difference
    /// between "this server has never had a user" and "this server has no active users". A
    /// server whose sole account was deactivated must **not** hand the next signup the
    /// `system_admin` role, and dropping the flag is exactly how it would. Note also that bots
    /// *are* excluded (`IncludeBotAccounts` is false), where
    /// [`mm_store::UserStore::is_empty`]'s `exclude_bots` parameter has to be passed explicitly
    /// — two adjacent "is this server fresh" questions with two different bot rules.
    pub async fn is_first_user_account(&self) -> bool {
        let options = mm_model::user_count::UserCountOptions {
            include_deleted: true,
            ..mm_model::user_count::UserCountOptions::default()
        };
        match self.store().user().count(&options).await {
            Ok(count) => count == 0,
            Err(err) => {
                tracing::error!(error = %err, "failed to count users for IsFirstUserAccount");
                false
            }
        }
    }

    /// Port of `App.CreateUserFromSignup` (app/user.go:286) — the anonymous branch of
    /// `createUser`, reached when the request carries neither `t` nor `iid` and the caller is not
    /// a system admin.
    ///
    /// # Three gates, in this order
    ///
    /// 1. [`App::is_user_signup_allowed`] — 501;
    /// 2. `!IsFirstUserAccount() && !EnableOpenServer` — **403**
    ///    `api.user.create_user.no_open_server`, whose `DetailedError` is `"email=<address>"`;
    /// 3. `EmailVerified = false`, unconditionally — a client cannot self-certify its address on
    ///    this path, and `SanitizeInput(false)` has already cleared the flag once. Go sets it
    ///    again anyway and so does this.
    ///
    /// The order matters: a closed server with sign-ups disabled answers 501, not 403.
    #[tracing::instrument(skip_all, fields(first_account))]
    pub async fn create_user_from_signup(&self, user: &User) -> AppResult<User> {
        self.is_user_signup_allowed()?;

        let first = self.is_first_user_account().await;
        tracing::Span::current().record("first_account", first);
        if !first && !self.config().enable_open_server {
            return Err(AppError::boxed(
                "CreateUserFromSignup",
                "api.user.create_user.no_open_server",
                None,
                format!("email={}", user.email),
                403,
            ));
        }

        let mut user = user.clone();
        user.email_verified = false;

        let ruser = self.create_user(&user).await?;
        // `SendWelcomeEmail` — see the module docs and [D-450].
        tracing::info!(user_id = %ruser.id, "welcome e-mail not sent: no e-mail service");
        Ok(ruser)
    }

    /// Port of `App.CreateUserAsAdmin` (app/user.go:273).
    ///
    /// **No gates at all.** It does not call `IsUserSignUpAllowed`, does not look at
    /// `EnableOpenServer`, and does not touch `EmailVerified` — so a system admin can create an
    /// account with `email_verified: true` on a server where signup is switched off entirely,
    /// and `SanitizeInput(true)` is what let the flag through. The permission that reaches this
    /// function is the whole authorisation.
    #[tracing::instrument(skip_all)]
    pub async fn create_user_as_admin(&self, user: &User) -> AppResult<User> {
        let ruser = self.create_user(user).await?;
        tracing::info!(user_id = %ruser.id, "welcome e-mail not sent: no e-mail service");
        Ok(ruser)
    }

    /// Port of `App.CreateUser` (app/user.go:324) → `createUserOrGuest(rctx, user, false)`
    /// (app/user.go:334), with `users.UserService.CreateUser` (app/users/users.go:26) inlined
    /// because the two halves share the same mutated `*model.User` and splitting them would need
    /// the mutation to cross a crate boundary.
    ///
    /// # The order of the refusals is observable
    ///
    /// `isAtUserLimit` → group-name collision → accepted domain → password → store. A signup that
    /// violates several reports the first, so reordering them changes the id a client sees even
    /// though every one of them is a 400.
    ///
    /// # Roles are assigned here, not accepted from the body
    ///
    /// `user.Roles` is **overwritten** with `system_user` before anything else, so a request body
    /// asking for `system_admin` is discarded — and then possibly replaced by
    /// `"system_admin system_user"` when [`mm_store::UserStore::is_empty`] says this is the first
    /// account. Note the order inside that string: admin first, and it is a space-joined pair,
    /// not just `system_admin`.
    ///
    /// # `is_bot` comes off the wire
    ///
    /// `User.IsBot` has a `json:` tag and `SanitizeInput` does not clear it, so a request can set
    /// it — and Go then **skips the first-account check** for that request. It does not make the
    /// row a bot (nothing writes `Bots`), so the effect is only to deny that account the admin
    /// role. Reproduced rather than hardened: the divergence would be a client that becomes an
    /// administrator here and does not on Go.
    #[tracing::instrument(skip_all, fields(username = %user.username, roles, locale_reset = false))]
    pub async fn create_user(&self, user: &User) -> AppResult<User> {
        // `isAtUserLimit` (app/limits.go:114). The licensed arm of the error is unreachable here:
        // `GetServerLimits` only sets a hard limit from a licence when one is installed, and the
        // api layer forwards a licensed server before it gets this far.
        let limits = self.get_server_limits(true).await?;
        if limits.max_users_hard_limit != 0
            && limits.active_user_count >= limits.max_users_hard_limit
        {
            return Err(AppError::boxed(
                "createUserOrGuest",
                "api.user.create_user.user_limits.exceeded",
                None,
                String::new(),
                400,
            ));
        }

        self.is_unique_to_group_names(&user.username)
            .await
            .map_err(|mut err| {
                // Go reassigns `err.Where` rather than building a new error, so the id and the
                // status survive and only the `Where` moves.
                err.where_ = "createUserOrGuest".to_owned();
                err
            })?;

        let mut user = user.clone();

        user.roles = SYSTEM_USER_ROLE_ID.to_owned();

        // `!IsLDAPUser() && !IsSAMLUser() && !IsGuest() && !CheckUserDomain(...)`. `IsGuest` is
        // false by construction — `Roles` was just set to `system_user` — so the guest-domain
        // branch below it in Go is dead on this path and is not written out.
        if !user.is_ldap_user()
            && !user.is_saml_user()
            && !check_email_domain(&user.email, &self.config().restrict_creation_to_domains)
        {
            return Err(AppError::boxed(
                "createUserOrGuest",
                "api.user.create_user.accepted_domain.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if !user.is_bot {
            match self.store().user().is_empty(true).await {
                Ok(true) => {
                    user.roles = format!("{SYSTEM_ADMIN_ROLE_ID} {SYSTEM_USER_ROLE_ID}");
                }
                Ok(false) => {}
                Err(err) => {
                    // Go wraps this as `UserStoreIsEmptyError` and the app layer turns it into a
                    // **500** `app.user.store_is_empty.app_error` — one of the few store failures
                    // on this path that is not a 400.
                    tracing::error!(error = %err, "the first-account check failed");
                    return Err(AppError::boxed(
                        "createUserOrGuest",
                        "app.user.store_is_empty.app_error",
                        None,
                        String::new(),
                        500,
                    ));
                }
            }
        }
        tracing::Span::current().record("roles", user.roles.as_str());

        // The locale reset. Membership of the **supported** list, not `model.IsValidLocale` —
        // see [`crate::i18n::SUPPORTED_LOCALES`]. The empty string is not supported either, so a
        // body that omits `locale` gets the default rather than staying empty.
        if !crate::i18n::is_supported_locale(&user.locale) {
            tracing::Span::current().record("locale_reset", true);
            user.locale = self.config().default_client_locale.clone();
        }

        // `users.createUser` from here (app/users/users.go:63).
        user.make_non_nil();

        // `if err := us.isPasswordValid(user.Password); user.AuthService == "" && err != nil` —
        // Go evaluates the *validation* first and the guard second, which is only a difference if
        // the validator had side effects. It has none, so the short-circuit here is equivalent.
        if user.auth_service.is_empty() {
            self.is_password_valid(&user.password).map_err(|mut err| {
                // `model.NewAppError("createUserOrGuest", nfErr.Id(), {"Min": …}, "", 400)` — the
                // id and the `Min` parameter come straight from the validator, which this port
                // already populates; only the `Where` changes.
                err.where_ = "createUserOrGuest".to_owned();
                err.status_code = 400;
                err
            })?;
        }

        let mut ruser = self
            .store()
            .user()
            .save(&user, &crate::password::latest_hasher())
            .await
            .map_err(create_user_save_error)?;

        if user.email_verified {
            // Go logs and continues: a user whose e-mail could not be flagged is still created.
            if let Err(err) = self
                .store()
                .user()
                .verify_email(&ruser.id, &user.email)
                .await
            {
                tracing::warn!(error = %err, "Failed to set email verified");
            }
        }

        // `ruser.DisableWelcomeEmail = user.DisableWelcomeEmail` — the store's returned row does
        // not carry it (it is not a column), so the field is copied back from the request.
        ruser.disable_welcome_email = user.disable_welcome_email;
        ruser.sanitize(&std::collections::HashMap::new());

        // `if user.EmailVerified` — the **request's** flag, not the saved row's. Go re-reads the
        // user so the event carries the verified address rather than the pre-write copy.
        if user.email_verified {
            match self.get_user(&ruser.id).await {
                Ok(fresh) => self.send_updated_user_event(&fresh).await,
                Err(err) => return Err(err),
            }
        }

        let preferences = Preferences(vec![
            Preference {
                user_id: ruser.id.clone(),
                category: PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS.to_owned(),
                name: PREFERENCE_NAME_RECOMMENDED_NEXT_STEPS_HIDE.to_owned(),
                value: "false".to_owned(),
            },
            Preference {
                user_id: ruser.id.clone(),
                category: PREFERENCE_CATEGORY_TUTORIAL_STEPS.to_owned(),
                // The **user's own id** as the preference name, which is how the tutorial step is
                // keyed. Not a constant.
                name: ruser.id.clone(),
                value: "0".to_owned(),
            },
            Preference {
                user_id: ruser.id.clone(),
                category: PREFERENCE_CATEGORY_SYSTEM_NOTICE.to_owned(),
                name: "GMasDM".to_owned(),
                value: "true".to_owned(),
            },
        ]);
        if let Err(err) = self.store().preference().save(&preferences).await {
            // Go warns and carries on — the account exists whether or not its preferences do.
            tracing::warn!(error = %err, "Encountered error saving user preferences");
        }

        // `go a.UpdateViewedProductNoticesForNewUser(ruser.Id)` is not ported: the product-notice
        // machinery is not in this tree, it runs in a goroutine whose result never reaches the
        // response, and it writes only to `ProductNoticeViewState`.

        // "This message goes to everyone, so the teamID, channelID and userID are irrelevant" —
        // an unaddressed broadcast carrying only `user_id`, *not* the user object. A client
        // fetches the profile itself.
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_NEW_USER,
            "",
            "",
            "",
            None,
            "",
        );
        message.add("user_id", serde_json::Value::from(ruser.id.as_str()));
        self.publish(message).await;

        // The trailing `GetServerLimits` is a **log line only** in Go — it cannot fail the
        // request, and the limit it warns about is the soft one, which the hard-limit check at
        // the top of this function has already let through.
        match self.get_server_limits(true).await {
            Ok(limits) => {
                if limits.max_users_limit > 0 && limits.active_user_count > limits.max_users_limit {
                    tracing::warn!(
                        user_limit = limits.max_users_limit,
                        "ERROR_SAFETY_LIMITS_EXCEEDED: Created user exceeds the total activated users limit.",
                    );
                }
            }
            Err(err) => {
                tracing::error!(error = %err, "Error fetching user limits in createUserOrGuest");
            }
        }

        Ok(ruser)
    }
}

/// Port of `users.CheckEmailDomain` (app/users/utils.go:18).
///
/// **An empty list allows everything**, which is the stock configuration — so this function is a
/// no-op on a default server and a port that inverted the empty case would refuse every signup.
///
/// The match is a case-insensitive suffix test against `"@" + domain`, not a parse: a list
/// entry of `example.com` therefore also admits `bob@evil-example.com`… no, it does not — the
/// `@` is part of the needle. It *does* admit `bob@sub.example.com` only if `sub.example.com` is
/// itself listed, since the suffix must start at the `@`.
fn check_email_domain(email: &str, domains: &str) -> bool {
    if domains.is_empty() {
        return true;
    }
    let lowered = mm_model::utils::go_to_lower(email);
    crate::team::normalize_domains(domains)
        .iter()
        .any(|domain| lowered.ends_with(&format!("@{domain}")))
}

/// `createUserOrGuest`'s five shapes for a failed `User().Save` (app/user.go:352-377).
///
/// The same five as `App.CreateBot`'s, with a different `Where` — and one extra arm above them
/// that Go reaches through `errors.Is(nErr, users.AcceptedDomainError)`; that one is raised
/// before the store here, so it is not in this function.
fn create_user_save_error(err: StoreError) -> Box<AppError> {
    match err {
        // `errors.As(nErr, &appErr)` — `PreSave` and `IsValid` keep their own id and status, so a
        // malformed username reports `model.user.is_valid.username.app_error` at 400 rather than
        // any create-specific id.
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { field, .. } => {
            let id = match field {
                "email" => "app.user.save.email_exists.app_error",
                "username" => "app.user.save.username_exists.app_error",
                _ => "app.user.save.existing.app_error",
            };
            AppError::boxed("createUserOrGuest", id, None, String::new(), 400)
        }
        other => {
            tracing::error!(error = %other, "the user row could not be saved");
            AppError::boxed(
                "createUserOrGuest",
                "app.user.save.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_domain_list_admits_everything() {
        assert!(check_email_domain("anyone@anywhere.test", ""));
    }

    #[test]
    fn the_suffix_must_start_at_the_at_sign() {
        assert!(check_email_domain("bob@example.com", "example.com"));
        // The needle is `@example.com`, so a look-alike domain that merely *ends* in the listed
        // string is refused. This is the branch a `contains` would get wrong.
        assert!(!check_email_domain("bob@evil-example.com", "example.com"));
        // And a subdomain is refused unless it is itself listed.
        assert!(!check_email_domain("bob@sub.example.com", "example.com"));
        assert!(check_email_domain("bob@sub.example.com", "sub.example.com"));
    }

    #[test]
    fn the_list_is_normalised_the_way_go_normalises_it() {
        // `@` and `,` become spaces, then lower-case, then split on whitespace.
        assert!(check_email_domain(
            "bob@example.org",
            "@corp.example.com, EXAMPLE.ORG"
        ));
        assert!(check_email_domain("BOB@EXAMPLE.ORG", "example.org"));
        assert!(!check_email_domain(
            "bob@example.net",
            "@corp.example.com, example.org"
        ));
    }

    /// The save-error map, branch by branch. The `_` arm is the one a reader is most likely to
    /// get wrong: it is `existing`, not `email_exists`.
    #[test]
    fn the_save_error_map_picks_the_id_from_the_field() {
        let of = |field| {
            create_user_save_error(StoreError::InvalidInput {
                entity: "User",
                field,
                value: String::new(),
            })
        };
        assert_eq!(of("email").id, "app.user.save.email_exists.app_error");
        assert_eq!(of("username").id, "app.user.save.username_exists.app_error");
        assert_eq!(of("id").id, "app.user.save.existing.app_error");
        assert_eq!(of("email").status_code, 400);
        assert_eq!(of("email").where_, "createUserOrGuest");
    }

    #[test]
    fn a_validation_failure_keeps_its_own_id_and_status() {
        let inner = AppError::boxed(
            "User.IsValid",
            "model.user.is_valid.username.app_error",
            None,
            String::new(),
            400,
        );
        let mapped = create_user_save_error(StoreError::Invalid {
            entity: "User",
            app_error: inner,
        });
        assert_eq!(mapped.id, "model.user.is_valid.username.app_error");
        assert_eq!(mapped.where_, "User.IsValid");
    }

    #[test]
    fn an_unrecognised_store_failure_is_a_five_hundred() {
        let mapped = create_user_save_error(StoreError::NotFound {
            entity: "User",
            criteria: "x".to_owned(),
        });
        assert_eq!(mapped.id, "app.user.save.app_error");
        assert_eq!(mapped.status_code, 500);
    }
}
