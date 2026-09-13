//! The user-update family behind `PUT /api/v4/users/{user_id}` and its three literal siblings
//! (`/patch`, `/active`, `/roles`) — app/user.go, app/role.go, app/auto_responder.go.
//!
//! # `updateUser` replaces, `patchUser` merges, and the store decides what neither may touch
//!
//! The two handlers differ in the body they take (`model.User` vs `model.UserPatch`) and almost
//! nowhere else: both end at `App.UpdateUser`, which ends at `SqlUserStore.Update` with
//! `trustedUpdateData = false`. That flag is the whole privilege story. Thirteen columns are
//! copied back off the stored row *unconditionally* — `CreateAt`, the auth pair, `Password`, the
//! two `Last*Update` stamps, `EmailVerified`, `FailedAttempts`, the MFA triple, `LastLogin` and
//! `RemoteId` — and with the flag false, `Roles` and `DeleteAt` join them. So a `PUT /users/{id}`
//! body claiming `"roles": "system_admin"`, `"delete_at": 0` or `"email_verified": true` changes
//! nothing: the submitted values are overwritten before the `UPDATE` is built.
//!
//! **`updateUser` does not call `SanitizeInput`.** Only `createUser` does (api4/user.go:246) —
//! grepped across the server tree, two call sites, neither of them here. The protection on this
//! route is the store's copy-back, not a scrub of the body, and a port that added a
//! `SanitizeInput` call would be *safe* but would not be Go: `SanitizeInput` zeroes `UpdateAt`,
//! and `PreUpdate` sets it from the clock regardless, so the visible difference is nil — which is
//! exactly why the missing call is easy to "fix" and worth pinning.
//!
//! What the body *can* still move is the seven profile columns, `Locale`, `Timezone`, `Props`,
//! `NotifyProps` and `AllowMarketing`. `Username` and `Email` additionally run the store's
//! uniqueness constraints and, for email, `App.UpdateUser`'s domain rules.
//!
//! # The email-change password check differs between the two routes
//!
//! Both require the current password when the *session owner* changes their own address. They
//! disagree about the failure:
//!
//! | | missing password | wrong password |
//! |---|---|---|
//! | `updateUser` | `DoubleCheckPassword` with `""` → `SetInvalidParam("password")`, 400 | same 400 |
//! | `patchUser` | `SetInvalidParam("password")`, 400 | **the error itself** — 401 for a bad credential, 401 for a locked-out account |
//!
//! So a wrong password answers 400 on one route and 401 on the other, for the same account and
//! the same mistake. `updateUser` swallows the lockout error too.
//!
//! And `DoubleCheckPassword` **writes** — it claims a `FailedAttempts` slot before it looks at the
//! password. Any decision to forward has to be made before it runs; every forward below is.

use mm_model::permission::{PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_ROLES};
use mm_model::role::{NEW_SYSTEM_ROLE_IDS, SYSTEM_ADMIN_ROLE_ID};
use mm_model::session::Session;
use mm_model::user::{User, UserPatch};
use mm_model::utils::{AppError, AppResult};
use mm_store::{SessionStore, UserStore};

use crate::App;
use crate::license::LicenseState;
use crate::post::PrepareError;

/// `model.TeamSettingsLockProfileFieldsNone` (config.go:149).
pub const LOCK_PROFILE_FIELDS_NONE: &str = "none";
/// `model.TeamSettingsLockProfileFieldsNameAndUsername` (config.go:150).
pub const LOCK_PROFILE_FIELDS_NAME_AND_USERNAME: &str = "name_and_username";

/// The five `lockedProfileField*` constants (app/user.go:38-42). **Two of them contain a space**
/// — `"first name"`, not `"first_name"` — and they reach the client inside the error's
/// `map[string]any{"Field": …}`, so the spelling is on the wire.
pub const LOCKED_PROFILE_FIELD_USERNAME: &str = "username";
pub const LOCKED_PROFILE_FIELD_FIRST_NAME: &str = "first name";
pub const LOCKED_PROFILE_FIELD_LAST_NAME: &str = "last name";
pub const LOCKED_PROFILE_FIELD_NICKNAME: &str = "nickname";
pub const LOCKED_PROFILE_FIELD_POSITION: &str = "position";

/// Port of `tryingToChange` (app/user.go:1392).
///
/// `patchValue != nil && *patchValue != *userValue`. An **absent** field is not a change, and a
/// field set to the value it already holds is not a change either — which is what makes a client
/// that round-trips the whole profile back (the webapp does) able to save an unrelated edit
/// without tripping every lock on the account.
#[must_use]
pub fn trying_to_change(user_value: &str, patch_value: Option<&String>) -> bool {
    patch_value.is_some_and(|value| value != user_value)
}

/// The field-by-field half of `App.CheckLockedProfileFields` (app/user.go:1425), with the
/// permission, licence and `AuthService` gates left to the caller.
///
/// Split out so the five branches below have tests: the three gates the caller holds are each
/// a short-circuit to "no conflict", and on an unlicensed server — the only kind this process can
/// speak for — the whole function is unreachable. Reproducing it anyway is the point; see
/// [`App::check_locked_profile_fields`].
///
/// # `name_and_username` locks three fields, `all` locks five
///
/// The setting is checked against **both** locking values first, so `"none"` (and anything
/// unrecognised) locks nothing. Then username, first name and last name always; nickname and
/// position only under `all`. The order is Go's and it is observable: a patch changing both the
/// username and the position on an `all` server is refused naming `username`.
///
/// # An empty first or last name may be filled in **once**
///
/// `user.FirstName != ""` guards the first-name branch and `user.LastName != ""` the last-name
/// one, so a user who signed up through a team invite with no name can set one and is then
/// locked out of changing it. The username branch has no such escape — a username is never
/// empty. Dropping either guard strands those accounts nameless, and it is invisible to any test
/// whose fixture user already has a name.
#[must_use]
pub fn locked_profile_field(setting: &str, user: &User, patch: &UserPatch) -> Option<&'static str> {
    if setting != LOCK_PROFILE_FIELDS_NAME_AND_USERNAME
        && setting != crate::config::TEAM_SETTINGS_LOCK_PROFILE_FIELDS_ALL
    {
        return None;
    }

    if trying_to_change(&user.username, patch.username.as_ref()) {
        return Some(LOCKED_PROFILE_FIELD_USERNAME);
    }

    if !user.first_name.is_empty() && trying_to_change(&user.first_name, patch.first_name.as_ref())
    {
        return Some(LOCKED_PROFILE_FIELD_FIRST_NAME);
    }
    if !user.last_name.is_empty() && trying_to_change(&user.last_name, patch.last_name.as_ref()) {
        return Some(LOCKED_PROFILE_FIELD_LAST_NAME);
    }

    if setting == crate::config::TEAM_SETTINGS_LOCK_PROFILE_FIELDS_ALL {
        if trying_to_change(&user.nickname, patch.nickname.as_ref()) {
            return Some(LOCKED_PROFILE_FIELD_NICKNAME);
        }
        if trying_to_change(&user.position, patch.position.as_ref()) {
            return Some(LOCKED_PROFILE_FIELD_POSITION);
        }
    }

    None
}

impl App {
    /// Port of `App.CheckProviderAttributes` (app/user.go:1397).
    ///
    /// Returns the name of the field the login provider owns, or `None`.
    ///
    /// # The username check is not inside the provider branch
    ///
    /// `user.AuthService != ""` covers **every** non-email account — LDAP, SAML, GitLab, Google,
    /// Office365, OpenID and the magic-link guest — and it refuses a username change for all of
    /// them before any provider object is consulted. That is the one arm of this function a
    /// server without the enterprise providers can still answer in full, and it is the arm a
    /// reader is most likely to fold into the LDAP branch below it.
    ///
    /// # Why an LDAP or SAML user forwards on a licensed server and not on this one
    ///
    /// `a.Ldap()` and `a.Saml()` are enterprise interfaces, `nil` until the corresponding
    /// licensed module registers itself. With both nil, Go's `if / else if / else if` chain falls
    /// straight through to `user.IsOAuthUser()`, which an LDAP or SAML account is not — so the
    /// answer is "no conflict". That is a genuine Go behaviour on an unlicensed server, not an
    /// approximation of one. On a licensed server the branch calls into the LDAP/SAML attribute
    /// map, which is not visible from this process, so those two accounts are
    /// [`PrepareError::Unreproducible`] and the handler forwards.
    ///
    /// The OAuth arm is portable either way: it is `"full name"` — one string for two fields,
    /// with a space — when either name field is being changed.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn check_provider_attributes(
        &self,
        user: &User,
        patch: &UserPatch,
    ) -> Result<Option<&'static str>, PrepareError> {
        if !user.auth_service.is_empty()
            && trying_to_change(&user.username, patch.username.as_ref())
        {
            return Ok(Some("username"));
        }

        if user.is_ldap_user() || user.is_saml_user() {
            match self.license_state().await? {
                LicenseState::Unlicensed => {}
                LicenseState::Licensed => {
                    return Err(PrepareError::Unreproducible(
                        "the LDAP and SAML provider-attribute maps are not visible here",
                    ));
                }
            }
        } else if user.is_oauth_user()
            && (trying_to_change(&user.first_name, patch.first_name.as_ref())
                || trying_to_change(&user.last_name, patch.last_name.as_ref()))
        {
            return Ok(Some("full name"));
        }

        Ok(None)
    }

    /// Port of `App.CheckLockedProfileFields` (app/user.go:1425).
    ///
    /// Three gates, then [`locked_profile_field`]. The gates are a disjunction in Go — any one of
    /// them means "no conflict" — so they are reordered here to put the two that cost nothing
    /// first and the licence last, exactly as [`App::is_profile_image_locked_for_user`] does and
    /// for the same reason: reordering pure predicates changes no answer, and it turns the
    /// licence from an unconditional forward into one that only fires when the other gates have
    /// already been passed.
    ///
    /// The *whole* field scan is hoisted above the licence for the same reason. It is below the
    /// licence in Go and it is pure, so a server left at the default `"none"` — and any patch
    /// that touches no locked field on any server — answers `None` without asking about the
    /// licence at all. Only a patch Go would actually refuse reaches the licence question, and
    /// that is the one this process cannot answer: `MinimumEnterpriseLicense` needs the SKU tier,
    /// so a Professional licence locks nothing and an Enterprise one locks everything, and the
    /// two are indistinguishable from here.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn check_locked_profile_fields(
        &self,
        session: &Session,
        user: &User,
        patch: &UserPatch,
    ) -> Result<Option<&'static str>, PrepareError> {
        if self
            .session_has_permission_to(session, &PERMISSION_EDIT_OTHER_USERS)
            .await
        {
            return Ok(None);
        }
        // `user.AuthService != ""` — the lock is for email/password accounts only.
        if !user.auth_service.is_empty() {
            return Ok(None);
        }

        // The field scan is hoisted above the licence, which is the reordering that matters: a
        // patch that conflicts with *nothing* has the same answer under every licence, so only a
        // request that would actually be refused has to ask a question this process cannot
        // answer.
        let setting = self.config().lock_profile_fields_for_email_users.clone();
        if locked_profile_field(&setting, user, patch).is_none() {
            return Ok(None);
        }

        match self.license_state().await? {
            // `MinimumEnterpriseLicense(nil)` is false, so the lock never applies.
            LicenseState::Unlicensed => Ok(None),
            LicenseState::Licensed => Err(PrepareError::Unreproducible(
                "the profile-field lock needs the licence SKU tier, which is not visible here",
            )),
        }
    }

    /// Port of `App.UpdateUserAsUser` (app/user.go:1381).
    ///
    /// A one-line wrapper in Go — `UpdateUser(user, true)` — and the `asAdmin` argument it takes
    /// is **unused**. Kept as its own function, and without the argument, because the name is
    /// what `updateUser` calls and a reader looking for it should find it.
    pub async fn update_user_as_user(&self, user: &User) -> AppResult<User> {
        self.update_user(user, true).await
    }

    /// Port of `App.PatchUser` (app/user.go:1472).
    ///
    /// Load, [`User::patch`], `UpdateUser`. The load is what makes this a merge: every field the
    /// patch leaves `nil` keeps the value on the stored row, where `updateUser` would have
    /// written whatever the body carried — including an empty string.
    ///
    /// `asAdmin` is unused here too, and is not reproduced.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn patch_user(&self, user_id: &str, patch: &UserPatch) -> AppResult<User> {
        let mut user = self.get_user(user_id).await?;
        user.patch(patch);
        self.update_user(&user, true).await
    }

    /// Port of `App.SetAutoResponderStatus` (app/auto_responder.go:87).
    ///
    /// Called by `patchUser` **after** the write, with the *old* notify props. Two transitions
    /// and no others: off→on sets the status out-of-office, on→off sets it online with
    /// `manual = true`. A patch that leaves `auto_responder_active` where it was does nothing —
    /// including when both are `"true"`, which is why the comparison is on the transition rather
    /// than on the new value.
    ///
    /// `SetStatusOutOfOffice` is not ported, so the off→on transition is
    /// [`PrepareError::Unreproducible`] and `patchUser` forwards the request rather than taking
    /// it. The forward is decided before the write; see `mm_api::user_updates::patch_user`.
    pub async fn set_auto_responder_status(
        &self,
        user: &User,
        old_notify_props: Option<&mm_model::utils::StringMap>,
    ) -> Result<(), PrepareError> {
        if auto_responder_turns_on(user.notify_props.as_ref(), old_notify_props) {
            return Err(PrepareError::Unreproducible(
                "SetStatusOutOfOffice is not ported",
            ));
        }
        if auto_responder_turns_off(user.notify_props.as_ref(), old_notify_props) {
            self.set_status_online(&user.id, true).await;
        }
        Ok(())
    }
}

impl App {
    /// Port of `App.CheckRolesExist` (app/role.go:257).
    ///
    /// Every name must come back from `GetRolesByNames`, and the **first** missing one names the
    /// error. Go loops over the *requested* names rather than diffing the sets, so a duplicate in
    /// the request is checked twice and a role the store returned but nobody asked for is
    /// ignored; both fall out of the loop shape rather than being decided.
    ///
    /// A name the store does not know is a **400**, `app.role.check_roles_exist.role_not_found`,
    /// with `role=<name>` as the detail — so `updateUserRoles` distinguishes "not a role name"
    /// (`IsValidUserRoles`, which is pure syntax) from "no such role" (this, which is a query).
    #[tracing::instrument(skip(self))]
    pub async fn check_roles_exist(&self, role_names: &[String]) -> AppResult<()> {
        let roles = self.get_roles_by_names(role_names).await?;

        for name in role_names {
            if !roles.iter().any(|role| &role.name == name) {
                return Err(AppError::boxed(
                    "CheckRolesExist",
                    "app.role.check_roles_exist.role_not_found",
                    None,
                    format!("role={name}"),
                    400,
                ));
            }
        }

        Ok(())
    }

    /// Port of `App.UpdateUserRoles` (app/user.go:2056).
    ///
    /// The only thing it adds to [`App::update_user_roles_with_user`] is the load — and a
    /// **status rewrite**: `err.StatusCode = http.StatusBadRequest` turns `GetUser`'s 404 for an
    /// unknown id into a 400, keeping the id `app.user.missing_account.const`. So this route
    /// answers 400 where every other route answers 404 for the same missing user.
    pub async fn update_user_roles(
        &self,
        user_id: &str,
        new_roles: &str,
        send_websocket_event: bool,
    ) -> AppResult<User> {
        let user = self.get_user(user_id).await.map_err(|mut err| {
            err.status_code = 400;
            err
        })?;
        self.update_user_roles_with_user(&user, new_roles, send_websocket_event)
            .await
    }

    /// Port of `App.UpdateUserRolesWithUser` (app/user.go:2066).
    ///
    /// # The last-administrator guard
    ///
    /// Fires only when the *target* is currently a system admin and the new list does not contain
    /// `system_admin`. The containment test is `strings.Contains` on the raw string, not a field
    /// scan — so `"system_administrator"` would satisfy it, which is Go's looseness and is
    /// reproduced. The count is the one in
    /// [`UserStore::count_system_admins`][mm_store::UserStore::count_system_admins], and `count <= 1`
    /// refuses: at that point the target *is* the count, so one is the last one.
    ///
    /// # `trustedUpdateData = true`
    ///
    /// The store copies `Roles` back off the stored row when the flag is false, which is what
    /// stops `updateUser` from granting anybody anything. This is the route that is *supposed*
    /// to change roles, so it passes `true` — and that also stops `DeleteAt` being copied back,
    /// which is harmless here because `user` was read from the row a moment ago.
    ///
    /// # The session write is best-effort, and it runs concurrently with the user write
    ///
    /// Go launches both in goroutines and only *logs* a session failure, "since the user roles
    /// were still updated". Reproduced as a sequential user write followed by a logged session
    /// write: the concurrency is not observable, but the asymmetry is — a client whose session
    /// update failed sees a 200 and keeps its old roles until the session is reloaded.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, roles = new_roles))]
    pub async fn update_user_roles_with_user(
        &self,
        user: &User,
        new_roles: &str,
        send_websocket_event: bool,
    ) -> AppResult<User> {
        let names: Vec<String> = new_roles.split_whitespace().map(str::to_owned).collect();
        self.check_roles_exist(&names).await?;

        if user.is_system_admin() && !new_roles.contains(SYSTEM_ADMIN_ROLE_ID) {
            let count = self
                .store()
                .user()
                .count_system_admins()
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "counting system admins failed");
                    AppError::boxed(
                        "UpdateUserRoles",
                        "app.user.update.countAdmins.app_error",
                        None,
                        String::new(),
                        400,
                    )
                })?;
            if count <= 1 {
                return Err(AppError::boxed(
                    "UpdateUserRoles",
                    "app.user.update.lastAdmin.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }
        }

        let mut user = user.clone();
        user.roles = new_roles.to_owned();

        let update = self
            .store()
            .user()
            .update(&user, true)
            .await
            .map_err(|err| update_roles_error(err, &user.id))?;

        if let Err(err) = self
            .store()
            .session()
            .update_roles(&user.id, new_roles)
            .await
        {
            // "soft error since the user roles were still updated"
            tracing::warn!(error = %err, user_id = %user.id, "Failed during updating user roles");
        }

        if send_websocket_event {
            let mut message = mm_model::websocket_message::WebSocketEvent::new(
                mm_model::websocket_message::WEBSOCKET_EVENT_USER_ROLE_UPDATED,
                "",
                "",
                &user.id,
                None,
                "",
            );
            message.add("user_id", serde_json::Value::String(user.id.clone()));
            message.add("roles", serde_json::Value::String(new_roles.to_owned()));
            self.publish(message).await;
        }

        Ok(update.new)
    }

    /// Port of `App.isAtUserLimit` (app/limits.go:114).
    ///
    /// `MaxUsersHardLimit == 0` means "no limit" and answers false; otherwise the test is
    /// `>=`, not `>`, so a server exactly at the hard limit refuses the next activation.
    pub async fn is_at_user_limit(&self) -> AppResult<bool> {
        let limits = self.get_server_limits(true).await?;
        if limits.max_users_hard_limit == 0 {
            return Ok(false);
        }
        Ok(limits.active_user_count >= limits.max_users_hard_limit)
    }

    /// Port of `App.UpdateActive` (app/user.go:1229), **activation only**.
    ///
    /// # `UpdateAt` is written here and overwritten in the store
    ///
    /// `SqlUserStore.Update` calls `PreUpdate`, which re-stamps `UpdateAt` from the clock
    /// (model/user.go:563), so the assignment below reaches no column of its own — on the
    /// activation arm `DeleteAt` is the constant `0` and nothing is copied off it at all. The
    /// deactivation arm is where the seeding matters; see [`App::deactivate_user`](crate::App::deactivate_user).
    ///
    /// # Why deactivation is not here
    ///
    /// `active = false` continues into `RevokeAllSessions` and `userDeactivated`, and the latter
    /// removes the user's OAuth auth data and access data, disables the bots they own and DMs
    /// the system admins about it — three store families this process does not have. All of it
    /// runs **after** the row is written, so there is no prefix of it to serve. The handler
    /// forwards the whole request instead; see [D-461].
    ///
    /// # The user-limit refusal is the first thing, and it is a licensed/unlicensed fork
    ///
    /// Two error ids for the same condition — `app.user.update_active.license_user_limit.exceeded`
    /// when a licence is installed and `…user_limit.exceeded` when not. Only the unlicensed one
    /// is reachable here, because [`App::get_server_limits`] speaks for an unlicensed server and
    /// the handler forwards a licensed one.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn activate_user(&self, user: &User) -> AppResult<User> {
        if self.is_at_user_limit().await? {
            return Err(AppError::boxed(
                "UpdateActive",
                "app.user.update_active.user_limit.exceeded",
                None,
                String::new(),
                400,
            ));
        }

        let mut user = user.clone();
        user.update_at = mm_model::utils::get_millis();
        user.delete_at = 0;

        let update = self
            .store()
            .user()
            .update(&user, true)
            .await
            .map_err(|err| update_active_error(err, &user.id))?;

        let new_user = update.new;
        self.send_updated_user_event(&new_user).await;
        Ok(new_user)
    }
}

/// The four error shapes `App.UpdateUserRolesWithUser` gives the store's failures
/// (app/user.go:2100). Same shape as `UpdateUser`'s, one `Where` and **no conflict arm** — a
/// role change writes no unique column, so Go never tests for one and a conflict here falls to
/// the 500.
fn update_roles_error(err: mm_store::StoreError, user_id: &str) -> Box<AppError> {
    match err {
        mm_store::StoreError::Invalid { app_error, .. } => app_error,
        mm_store::StoreError::InvalidInput { .. } => AppError::boxed(
            "UpdateUserRoles",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, user_id, "user role update failed");
            AppError::boxed(
                "UpdateUserRoles",
                "app.user.update.finding.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `App.UpdateActive`'s three store-error shapes (app/user.go:1253) — the same triple as
/// `UpdateUserRoles`, under a different `Where`.
fn update_active_error(err: mm_store::StoreError, user_id: &str) -> Box<AppError> {
    match err {
        mm_store::StoreError::Invalid { app_error, .. } => app_error,
        mm_store::StoreError::InvalidInput { .. } => AppError::boxed(
            "UpdateActive",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, user_id, "user activation failed");
            AppError::boxed(
                "UpdateActive",
                "app.user.update.finding.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// Whether `newRoles` names a role that only an enterprise licence may assign
/// (api4/user.go:1740).
///
/// Go splits on `strings.FieldsSeq` and compares each field against `model.NewSystemRoleIDs`
/// exactly — the four system-console roles. The licence question that follows is
/// `*license.Features.CustomPermissionsSchemes`, which is not visible from this process, so a
/// caller naming one of these on a licensed server is forwarded and on an unlicensed one is the
/// 400 Go gives a `nil` licence.
#[must_use]
pub fn roles_need_custom_permissions_schemes(new_roles: &str) -> bool {
    new_roles
        .split_whitespace()
        .any(|role| NEW_SYSTEM_ROLE_IDS.contains(&role))
}

/// `PermissionManageRoles`, re-exported so the handler does not have to reach past this module
/// for the one permission this family adds.
pub const MANAGE_ROLES: &mm_model::permission::Permission = &PERMISSION_MANAGE_ROLES;

/// `NotifyProps[auto_responder_active] == "true"` (app/auto_responder.go:88).
///
/// An **absent** key is false, and so is any value but the exact string `"true"` — `"True"` and
/// `"1"` are both off. A nil map is false rather than a panic, which matters because
/// `model.User.NotifyProps` is genuinely nil on a row that predates `SetDefaultNotifications`.
#[must_use]
fn auto_responder_flag(props: Option<&mm_model::utils::StringMap>) -> bool {
    props
        .and_then(|props| props.get(mm_model::user::AUTO_RESPONDER_ACTIVE_NOTIFY_PROP))
        .is_some_and(|value| value == "true")
}

/// The off→on transition of `SetAutoResponderStatus`, as a pure predicate so `patchUser` can ask
/// about it **before** it writes. `SetStatusOutOfOffice` is unported, so this arm forwards.
#[must_use]
pub fn auto_responder_turns_on(
    new_notify_props: Option<&mm_model::utils::StringMap>,
    old_notify_props: Option<&mm_model::utils::StringMap>,
) -> bool {
    !auto_responder_flag(old_notify_props) && auto_responder_flag(new_notify_props)
}

/// The on→off transition. Go's two arms are `!oldActive && active` and `oldActive && !active`,
/// which is *not* `old != new` written twice — both are false when the flag is unchanged, and
/// writing either as a negation of the other would make an unchanged `"true"` set a status.
#[must_use]
pub fn auto_responder_turns_off(
    new_notify_props: Option<&mm_model::utils::StringMap>,
    old_notify_props: Option<&mm_model::utils::StringMap>,
) -> bool {
    auto_responder_flag(old_notify_props) && !auto_responder_flag(new_notify_props)
}

/// Parity tests for the pure predicates of this family, driven by
/// `fixtures/behaviour_user_update.json`.
///
/// `CheckLockedProfileFields` and `tryingToChange` are unexported methods on `*App`, so the
/// generator copies them verbatim rather than calling them — see the header of
/// `reference/dump/behaviour_user_update.go`. Everything else in the corpus is a real call into
/// the `model` package.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_user_update.json"))
            .expect("the behaviour fixture parses")
    }

    fn cases(key: &str) -> Vec<serde_json::Value> {
        oracle()[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is an array"))
            .clone()
    }

    #[test]
    fn trying_to_change_matches_go() {
        let rows = cases("trying_to_change");
        assert!(rows.len() >= 7, "the corpus is populated");
        for row in rows {
            let user = row["user"].as_str().expect("a string").to_owned();
            let patch = row["patch"].as_str().map(str::to_owned);
            assert_eq!(
                trying_to_change(&user, patch.as_ref()),
                row["changed"].as_bool().expect("a bool"),
                "tryingToChange({user:?}, {patch:?})"
            );
        }
    }

    #[test]
    fn the_locked_profile_field_scan_matches_go() {
        let rows = cases("locked_profile");
        assert!(
            rows.len() >= 25,
            "one row per branch, per coincidence and per adjacent pair"
        );
        for row in rows {
            let name = row["name"].as_str().expect("a name");
            let setting = row["setting"].as_str().expect("a setting");
            let user: User =
                serde_json::from_value(row["user"].clone()).expect("the user deserialises");
            let patch: UserPatch =
                serde_json::from_value(row["patch"].clone()).expect("the patch deserialises");

            let expected = row["field"].as_str().expect("a field");
            let expected = if expected.is_empty() {
                None
            } else {
                Some(expected)
            };

            assert_eq!(
                locked_profile_field(setting, &user, &patch),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn the_auto_responder_transitions_match_go() {
        let rows = cases("auto_responder");
        assert_eq!(rows.len(), 64, "every pair of the eight prop shapes");
        let map = |value: &serde_json::Value| -> Option<mm_model::utils::StringMap> {
            serde_json::from_value(value.clone()).expect("a string map or null")
        };
        for row in rows {
            let old = map(&row["old"]);
            let next = map(&row["new"]);
            assert_eq!(
                auto_responder_turns_on(next.as_ref(), old.as_ref()),
                row["turns_on"].as_bool().expect("a bool"),
                "turns_on({:?} -> {:?})",
                row["old"],
                row["new"]
            );
            assert_eq!(
                auto_responder_turns_off(next.as_ref(), old.as_ref()),
                row["turns_off"].as_bool().expect("a bool"),
                "turns_off({:?} -> {:?})",
                row["old"],
                row["new"]
            );
        }
    }

    #[test]
    fn the_new_system_role_licence_gate_matches_go() {
        let rows = cases("new_system_roles");
        assert!(rows.len() >= 12, "the corpus is populated");
        for row in rows {
            let roles = row["roles"].as_str().expect("a string");
            assert_eq!(
                roles_need_custom_permissions_schemes(roles),
                row["needs_licence"].as_bool().expect("a bool"),
                "NewSystemRoleIDs membership of {roles:?}"
            );
            // The validity answer travels with it because `updateUserRoles` consults them in
            // that order and the two disagree: `"system_admin"` alone is *invalid* and needs no
            // licence, `"System_Manager"` is also invalid (the capital is outside
            // `IsValidRoleName`'s alphabet) and would not have matched anyway.
            assert_eq!(
                mm_model::user::is_valid_user_roles(roles),
                row["valid"].as_bool().expect("a bool"),
                "IsValidUserRoles({roles:?})"
            );
        }
    }

    /// The corpus rows a mutation of the field *order* in [`locked_profile_field`] has to move.
    ///
    /// Named explicitly because the other rows cannot see it: a patch that changes exactly one
    /// field gives the same answer under every ordering, so only the three rows that change two
    /// locked fields at once discriminate. If one of them is deleted, this fails rather than the
    /// order silently stopping being tested.
    #[test]
    fn the_corpus_has_rows_that_pin_the_field_order() {
        let rows = cases("locked_profile");
        for name in [
            "all username before first name",
            "all first name before last name",
            "all last name before nickname",
            "all nickname before position",
        ] {
            assert!(
                rows.iter().any(|row| row["name"] == name),
                "the corpus still has the {name:?} row"
            );
        }
    }
}
